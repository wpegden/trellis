//! Invocation-local source-key construction. No metadata or persistent cache is trusted.
use super::{PREAMBLE_NAME, extract_tablet_imports, hash_bytes, hash_text, repo_check_script_path};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

const MAX_INPUTS: usize = 16_384;
// Budget for input bytes processed by this optional pass. A single read uses
// the original fs::read semantics; this is not an allocation ceiling.
const MAX_BYTES: usize = 128 << 20;
const MAX_MEMBERS: usize = 1_000_000;
const LAKE_FILES: [&str; 4] = [
    "lakefile.lean",
    "lakefile.toml",
    "lake-manifest.json",
    "lean-toolchain",
];
struct Input {
    hash: Option<String>,
    imports: BTreeSet<String>,
}
pub(crate) struct SourceKeys {
    pub(crate) keys: BTreeMap<String, String>,
    pub(crate) closure: BTreeSet<String>,
    inputs: BTreeMap<PathBuf, Input>,
    common: String,
    bytes: usize,
}
impl SourceKeys {
    fn read(&mut self, path: PathBuf) -> Option<&Input> {
        if !self.inputs.contains_key(&path) {
            if self.inputs.len() >= MAX_INPUTS {
                return None;
            }
            let bytes = fs::read(&path).ok();
            self.bytes = self.bytes.checked_add(bytes.as_ref().map_or(0, Vec::len))?;
            if self.bytes > MAX_BYTES {
                return None;
            }
            let input = Input {
                hash: bytes.as_ref().map(|b| hash_bytes(b)),
                imports: bytes
                    .as_ref()
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .map(extract_tablet_imports)
                    .unwrap_or_default(),
            };
            self.inputs.insert(path.clone(), input);
        }
        self.inputs.get(&path)
    }
    fn source(repo: &Path, node: &str) -> PathBuf {
        repo.join("Tablet").join(format!("{node}.lean"))
    }
    fn reachable(&mut self, repo: &Path, node: &str) -> Option<BTreeSet<String>> {
        let mut pending = vec![node.to_string()];
        let mut closure = BTreeSet::new();
        while let Some(node) = pending.pop() {
            if node.is_empty() || !closure.insert(node.clone()) {
                continue;
            }
            if closure.len() > MAX_INPUTS {
                return None;
            }
            pending.extend(
                self.read(Self::source(repo, &node))?
                    .imports
                    .iter()
                    .cloned(),
            );
        }
        Some(closure)
    }
    pub(crate) fn capture(repo: &Path, nodes: &BTreeSet<String>) -> Option<Self> {
        Self::capture_with_member_limit(repo, nodes, MAX_MEMBERS)
    }
    fn capture_with_member_limit(
        repo: &Path,
        nodes: &BTreeSet<String>,
        member_limit: usize,
    ) -> Option<Self> {
        let mut out = Self {
            keys: BTreeMap::new(),
            closure: BTreeSet::new(),
            inputs: BTreeMap::new(),
            common: String::new(),
            bytes: 0,
        };
        for name in LAKE_FILES {
            let hash = out
                .read(repo.join(name))?
                .hash
                .clone()
                .unwrap_or_else(|| hash_bytes(&[]));
            out.common.push_str(&format!("lake:{name}={hash}\n"));
        }
        let script = out.read(repo_check_script_path(repo))?.hash.clone()?;
        out.common.push_str(&format!("script={script}\n"));
        let preamble = out.read(Self::source(repo, PREAMBLE_NAME))?.hash.clone()?;
        out.common.push_str(&format!("preamble.lean={preamble}\n"));
        for node in nodes {
            let reachable = out.reachable(repo, node)?;
            out.closure.extend(reachable);
        }
        let targets: BTreeSet<_> = nodes
            .union(&out.closure)
            .cloned()
            .chain([PREAMBLE_NAME.to_string()])
            .collect();
        let mut memberships = 0usize;
        for node in targets {
            let mut closure = out.reachable(repo, &node)?;
            memberships = memberships.checked_add(closure.len())?;
            if memberships > member_limit {
                return None;
            }
            closure.remove(&node);
            closure.remove(PREAMBLE_NAME);
            let own = out.read(Self::source(repo, &node))?.hash.clone()?;
            let mut blob = format!("cache_v=2\n{}self={node}={own}\n", out.common);
            for dep in closure {
                let hash = out.read(Self::source(repo, &dep))?.hash.clone()?;
                blob.push_str(&format!("dep:{dep}={hash}\n"));
            }
            out.keys.insert(node, hash_text(&blob));
        }
        Some(out)
    }
    /// Re-read all positive AND negative inputs. Changed imports, previously absent
    /// paths, changed bytes/readability and source edits cannot reuse this pass.
    /// Same-byte replacement remains valid under the existing content-only keys.
    pub(crate) fn current(&self) -> bool {
        self.inputs
            .iter()
            .all(|(path, old)| fs::read(path).ok().map(|b| hash_bytes(&b)) == old.hash)
    }
    pub(crate) fn multi_key(&self, nodes: &BTreeSet<String>) -> Option<String> {
        let mut blob = format!("multi_cache_v=1\n{}", self.common);
        for node in nodes {
            blob.push_str(&format!("node:{node}={}\n", self.keys.get(node)?));
        }
        Some(hash_text(&blob))
    }
}

/// Invocation-local source and compiled-artifact keys. Call `current` at the
/// consuming boundary; never retain this witness across checker operations.
/// Keys have exactly the historical per-node byte format and salts.
pub struct ResultKeys {
    pub keys: BTreeMap<String, String>,
    source: SourceKeys,
    artifacts: BTreeMap<PathBuf, ArtifactInput>,
}
#[derive(Clone, PartialEq, Eq)]
struct ArtifactInput {
    exists: bool,
    content: Option<(u64, String)>,
}
impl ArtifactInput {
    fn read(path: &Path) -> Self {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let exists = path.exists();
        let content = (|| {
            let mut file = fs::File::open(path).ok()?;
            let mut hash = Sha256::new();
            let mut bytes = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let n = file.read(&mut buffer).ok()?;
                if n == 0 {
                    break;
                }
                bytes = bytes.checked_add(n as u64)?;
                hash.update(&buffer[..n]);
            }
            Some((bytes, format!("{:x}", hash.finalize())))
        })();
        Self { exists, content }
    }
}
impl ResultKeys {
    /// Optional bounded batching. Failure simply selects historical per-node
    /// construction. Streaming artifact reads retain only lengths and SHA256s.
    pub fn capture(repo: &Path, nodes: &BTreeSet<String>) -> Option<Self> {
        Self::capture_with_member_limit(repo, nodes, MAX_MEMBERS)
    }
    fn capture_with_member_limit(
        repo: &Path,
        nodes: &BTreeSet<String>,
        member_limit: usize,
    ) -> Option<Self> {
        let mut source = SourceKeys::capture(repo, nodes)?;
        let mut artifacts = BTreeMap::new();
        let mut keys = BTreeMap::new();
        let mut memberships = 0usize;
        let dir = repo.join(".lake/build/lib/lean/Tablet");
        for node in nodes {
            let closure = source.reachable(repo, node)?;
            memberships = memberships.checked_add(closure.len())?;
            if memberships > member_limit {
                return None;
            }
            let mut blob = format!("olean_closure_v=2\nsource={}\n", source.keys.get(node)?);
            for member in closure {
                let exported = dir.join(format!("{member}.olean"));
                let server = PathBuf::from(format!("{}.server", exported.display()));
                let private = PathBuf::from(format!("{}.private", exported.display()));
                for path in [&exported, &server, &private] {
                    if !artifacts.contains_key(path) {
                        if artifacts.len() >= MAX_INPUTS * 3 {
                            return None;
                        }
                        artifacts.insert(path.clone(), ArtifactInput::read(path));
                    }
                }
                for (level, path, include) in [
                    ("exported", &exported, true),
                    ("server", &server, artifacts[&server].exists),
                    ("private", &private, artifacts[&private].exists),
                ] {
                    if !include {
                        continue;
                    }
                    match &artifacts[path].content {
                        Some((len, hash)) => {
                            blob.push_str(&format!("artifact:{member}:{level}:{len}={hash}\n"))
                        }
                        None => blob.push_str(&format!("artifact:{member}:{level}=<absent>\n")),
                    }
                }
                if artifacts[&private].exists && !artifacts[&server].exists {
                    blob.push_str(&format!(
                        "artifact:{member}=<unsupported-private-without-server>\n"
                    ));
                }
            }
            keys.insert(node.clone(), hash_text(&blob));
        }
        Some(Self {
            keys,
            source,
            artifacts,
        })
    }
    /// The checker may rebuild artifacts, but a changed source/config basis
    /// cannot authenticate an observation made against the earlier basis.
    pub fn same_sources(&self, other: &Self) -> bool {
        self.source.keys == other.source.keys
    }
    /// Content-only freshness, including optional artifact absence/read errors
    /// and all source/import/config positive and negative inputs. Same-byte
    /// inode replacement is valid, just as with the historical key function.
    pub fn current(&self) -> bool {
        self.source.current()
            && self
                .artifacts
                .iter()
                .all(|(path, old)| ArtifactInput::read(path) == *old)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("Tablet")).unwrap();
        fs::create_dir_all(d.path().join(".trellis/scripts")).unwrap();
        fs::write(d.path().join(".trellis/scripts/check.py"), "checker").unwrap();
        fs::write(
            d.path().join("Tablet/Preamble.lean"),
            "import Tablet.Root\n",
        )
        .unwrap();
        fs::write(d.path().join("Tablet/Root.lean"), "def root := 1\n").unwrap();
        fs::write(
            d.path().join("Tablet/A.lean"),
            "import Tablet.B Tablet.Preamble\n/- import Tablet.Absent -/\ndef a := 1\n",
        )
        .unwrap();
        fs::write(
            d.path().join("Tablet/B.lean"),
            "import Tablet.A\ndef b := 2\n",
        )
        .unwrap();
        d
    }
    #[test]
    fn exact_keys_cycles_preamble_and_order() {
        let d = repo();
        let nodes = ["A", "B"].map(str::to_string).into();
        let keys = SourceKeys::capture(d.path(), &nodes).unwrap();
        assert!(keys.current());
        for (node, key) in &keys.keys {
            assert_eq!(
                Some(key.clone()),
                super::super::lean_closure_cache_key(d.path(), node)
            );
        }
        assert_eq!(
            keys.multi_key(&nodes),
            super::super::lean_closure_cache_key_for_nodes_uncached(d.path(), &nodes)
        );
    }
    #[test]
    fn mutation_between_invocations_and_during_pass_invalidates() {
        for path in [
            "Tablet/A.lean",
            "Tablet/Root.lean",
            ".trellis/scripts/check.py",
            "lean-toolchain",
            "lakefile.toml",
        ] {
            let d = repo();
            let nodes = ["A".to_string()].into();
            let old = SourceKeys::capture(d.path(), &nodes).unwrap();
            fs::write(d.path().join(path), "changed same invocation\n").unwrap();
            assert!(!old.current(), "{path}");
            let new = SourceKeys::capture(d.path(), &nodes).unwrap();
            assert!(new.current());
            assert_eq!(
                new.multi_key(&nodes),
                super::super::lean_closure_cache_key_for_nodes_uncached(d.path(), &nodes)
            );
            assert_ne!(old.multi_key(&nodes), new.multi_key(&nodes), "{path}");
        }
    }
    #[test]
    fn removed_import_and_new_optional_file_invalidate() {
        let d = repo();
        let nodes = ["A".to_string()].into();
        let old = SourceKeys::capture(d.path(), &nodes).unwrap();
        fs::remove_file(d.path().join("Tablet/B.lean")).unwrap();
        assert!(!old.current());
        assert!(SourceKeys::capture(d.path(), &nodes).is_none());
        fs::write(d.path().join("Tablet/B.lean"), "import Tablet.New\n").unwrap();
        assert!(SourceKeys::capture(d.path(), &nodes).is_none());
        fs::write(d.path().join("Tablet/New.lean"), "def n := 0\n").unwrap();
        assert!(SourceKeys::capture(d.path(), &nodes).unwrap().current());
    }
    #[test]
    fn deletion_and_same_size_replacement_invalidate() {
        let d = repo();
        let nodes = ["A".to_string()].into();
        let old = SourceKeys::capture(d.path(), &nodes).unwrap();
        let p = d.path().join("Tablet/Root.lean");
        let bytes = fs::read(&p).unwrap();
        fs::write(&p, vec![b' '; bytes.len()]).unwrap();
        assert!(!old.current());
        fs::write(&p, &bytes).unwrap();
        assert!(old.current());
        fs::remove_file(&p).unwrap();
        assert!(!old.current());
    }
    #[test]
    fn overlapping_closures_charge_aggregate_membership() {
        let d = repo();
        let nodes = ["A".to_string(), "B".to_string()].into();
        assert!(SourceKeys::capture_with_member_limit(d.path(), &nodes, 4).is_none());
        assert!(SourceKeys::capture_with_member_limit(d.path(), &nodes, 100).is_some());
    }
    #[test]
    fn invalid_utf8_and_optional_absence_preserve_original_keys() {
        let d = repo();
        let nodes = ["A".to_string()].into();
        for path in ["Tablet/A.lean", "lakefile.toml"] {
            fs::write(d.path().join(path), [255, 254]).unwrap();
        }
        let keys = SourceKeys::capture(d.path(), &nodes).unwrap();
        assert!(keys.current());
        assert_eq!(
            keys.multi_key(&nodes),
            super::super::lean_closure_cache_key_for_nodes_uncached(d.path(), &nodes)
        );
        fs::write(d.path().join("lakefile.lean"), b"").unwrap();
        assert!(!keys.current());
        assert_eq!(
            super::super::lean_closure_cache_key_for_nodes(d.path(), &nodes),
            super::super::lean_closure_cache_key_for_nodes_uncached(d.path(), &nodes)
        );
    }
    fn artifacts(repo: &Path) -> PathBuf {
        let dir = repo.join(".lake/build/lib/lean/Tablet");
        fs::create_dir_all(&dir).unwrap();
        for node in ["A", "B", "Preamble", "Root"] {
            fs::write(
                dir.join(format!("{node}.olean")),
                format!("compiled:{node}"),
            )
            .unwrap();
        }
        dir
    }
    fn result_parity(repo: &Path, nodes: &BTreeSet<String>) -> ResultKeys {
        let keys = ResultKeys::capture(repo, nodes).unwrap();
        assert!(keys.current());
        for node in nodes {
            assert_eq!(
                keys.keys.get(node).cloned(),
                super::super::lean_closure_cache_key_with_oleans(repo, node),
                "{node}"
            );
        }
        keys
    }
    #[test]
    fn result_keys_exact_all_sidecar_presence_combinations() {
        for server in [false, true] {
            for private in [false, true] {
                let d = repo();
                let dir = artifacts(d.path());
                if server {
                    fs::write(dir.join("B.olean.server"), b"server").unwrap();
                }
                if private {
                    fs::write(dir.join("B.olean.private"), b"private").unwrap();
                }
                result_parity(d.path(), &["A".into(), "B".into()].into());
            }
        }
    }
    #[test]
    fn result_keys_exact_missing_and_unreadable_artifacts() {
        let d = repo();
        let dir = artifacts(d.path());
        fs::remove_file(dir.join("A.olean")).unwrap();
        // Directories exist but produce read errors, including optional inputs.
        for name in ["A.olean", "A.olean.server", "A.olean.private"] {
            fs::create_dir(dir.join(name)).unwrap();
        }
        result_parity(d.path(), &["A".into(), "B".into()].into());
    }
    #[test]
    fn result_keys_invalidate_created_removed_rewritten_artifacts() {
        for name in [
            "A.olean",
            "B.olean",
            "Root.olean",
            "B.olean.server",
            "B.olean.private",
        ] {
            let d = repo();
            let dir = artifacts(d.path());
            let nodes = ["A".into(), "B".into()].into();
            let before = result_parity(d.path(), &nodes);
            let path = dir.join(name);
            fs::write(&path, b"updated artifact").unwrap();
            assert!(!before.current(), "{name}");
            let after = result_parity(d.path(), &nodes);
            assert_ne!(before.keys, after.keys, "{name}");
            fs::remove_file(&path).unwrap();
            assert!(!after.current(), "removed {name}");
            result_parity(d.path(), &nodes);
        }
    }
    #[test]
    fn result_keys_invalidate_sources_imports_configs_and_checker() {
        for path in [
            "Tablet/A.lean",
            "Tablet/Root.lean",
            "lakefile.toml",
            ".trellis/scripts/check.py",
        ] {
            let d = repo();
            artifacts(d.path());
            let nodes = ["A".into()].into();
            let before = result_parity(d.path(), &nodes);
            fs::write(d.path().join(path), b"changed input").unwrap();
            assert!(!before.current(), "{path}");
            let after = result_parity(d.path(), &nodes);
            assert_ne!(before.keys, after.keys);
        }
        let d = repo();
        artifacts(d.path());
        let nodes = ["A".into()].into();
        let before = result_parity(d.path(), &nodes);
        fs::write(d.path().join("Tablet/B.lean"), b"import Tablet.New\n").unwrap();
        assert!(!before.current());
        assert!(ResultKeys::capture(d.path(), &nodes).is_none());
        fs::write(d.path().join("Tablet/New.lean"), b"def new := 1\n").unwrap();
        result_parity(d.path(), &nodes);
    }
    #[test]
    fn result_keys_budget_and_invalid_source_fall_back() {
        let d = repo();
        artifacts(d.path());
        let nodes = ["A".into(), "B".into()].into();
        assert!(ResultKeys::capture_with_member_limit(d.path(), &nodes, 1).is_none());
        assert!(ResultKeys::capture_with_member_limit(d.path(), &nodes, 100).is_some());
        fs::write(d.path().join("Tablet/A.lean"), [255, 254]).unwrap();
        result_parity(d.path(), &nodes);
        fs::remove_file(d.path().join("Tablet/B.lean")).unwrap();
        assert!(ResultKeys::capture(d.path(), &nodes).is_none());
    }
    #[test]
    fn result_keys_same_bytes_replacement_is_content_equivalent() {
        let d = repo();
        let dir = artifacts(d.path());
        let nodes = ["A".into()].into();
        let before = result_parity(d.path(), &nodes);
        let path = dir.join("B.olean");
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(path, bytes).unwrap();
        assert!(before.current());
        assert_eq!(before.keys, result_parity(d.path(), &nodes).keys);
    }
}
