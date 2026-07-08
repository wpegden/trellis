use crate::model::{Fingerprint, NodeId, TargetId};
use crate::paper_targets::{extract_tex_statement_items, TexStatementItem};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const PREAMBLE_NAME: &str = "Preamble";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct PaperTargetFingerprint {
    target: TargetId,
    covering_nodes: BTreeMap<NodeId, Fingerprint>,
    /// Hashes of `.tex` of every def-kind descendant whose Lean
    /// declaration is consumed by some covering node's
    /// `lean_semantic_closure` walk (union over coverage[T] of
    /// L_def(covering)). Narrower than the textual import closure:
    /// descendants reached only via proof bodies do not appear here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    lean_relevant_definition_descendants: BTreeMap<NodeId, Fingerprint>,
    preamble_definition_hashes: BTreeSet<Fingerprint>,
}

fn hash_text(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn read_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn tablet_dir(repo_path: &Path) -> PathBuf {
    repo_path.join("Tablet")
}

fn node_lean_path(repo_path: &Path, node: &str) -> PathBuf {
    tablet_dir(repo_path).join(format!("{node}.lean"))
}

fn node_tex_path(repo_path: &Path, node: &str) -> PathBuf {
    tablet_dir(repo_path).join(format!("{node}.tex"))
}

fn extract_tablet_imports(lean_content: &str) -> BTreeSet<NodeId> {
    lean_content
        .lines()
        .filter_map(|line| line.trim().strip_prefix("import Tablet."))
        .map(str::trim)
        .filter(|suffix| !suffix.is_empty())
        .map(NodeId::from)
        .collect()
}

fn direct_imports(repo_path: &Path, node: &str) -> BTreeSet<NodeId> {
    extract_tablet_imports(&read_text(&node_lean_path(repo_path, node)))
}

fn recursive_import_closure(repo_path: &Path, node: &str, visited: &mut BTreeSet<NodeId>) {
    if node.is_empty() || visited.contains(node) {
        return;
    }
    visited.insert(NodeId::from(node));
    if node == PREAMBLE_NAME {
        return;
    }
    for dep in direct_imports(repo_path, node) {
        recursive_import_closure(repo_path, &dep, visited);
    }
}

fn statement_item_hash(item: &TexStatementItem) -> Fingerprint {
    hash_text(
        &serde_json::to_string(item)
            .unwrap_or_else(|_| format!("{}|{}|{}", item.env, item.title, item.body)),
    )
}

fn node_statement_hash(repo_path: &Path, node: &str) -> Option<Fingerprint> {
    let items = extract_tex_statement_items(&read_text(&node_tex_path(repo_path, node)), false);
    items.first().map(statement_item_hash)
}

fn preamble_definition_hashes(repo_path: &Path) -> BTreeSet<Fingerprint> {
    extract_tex_statement_items(
        &read_text(&tablet_dir(repo_path).join("Preamble.tex")),
        true,
    )
    .into_iter()
    .filter(|item| item.env == "definition")
    .map(|item| statement_item_hash(&item))
    .collect()
}

fn decode_paper_target_fingerprint(raw: &str) -> Option<PaperTargetFingerprint> {
    if raw.trim().is_empty() {
        return None;
    }
    serde_json::from_str(raw).ok()
}

fn coverage_from_claims(
    configured_targets: &BTreeSet<TargetId>,
    target_claims: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeMap<TargetId, BTreeSet<NodeId>> {
    let mut coverage: BTreeMap<TargetId, BTreeSet<NodeId>> = configured_targets
        .iter()
        .cloned()
        .map(|target| (target, BTreeSet::new()))
        .collect();
    for node in present_nodes {
        if let Some(targets) = target_claims.get(node) {
            for target in targets {
                if configured_targets.contains(target) {
                    coverage
                        .entry(target.clone())
                        .or_default()
                        .insert(node.clone());
                }
            }
        }
    }
    coverage
}

pub fn observe_paper_faithfulness_fingerprints(
    repo_path: &Path,
    configured_targets: &BTreeSet<TargetId>,
    target_claims: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    present_nodes: &BTreeSet<NodeId>,
    approved_paper_fingerprints: &BTreeMap<TargetId, Fingerprint>,
    lean_relevant_descendants_per_covering: &BTreeMap<NodeId, BTreeSet<NodeId>>,
) -> BTreeMap<TargetId, Fingerprint> {
    let coverage = coverage_from_claims(configured_targets, target_claims, present_nodes);
    let current_preamble_hashes = preamble_definition_hashes(repo_path);

    configured_targets
        .iter()
        .map(|target| {
            let covering = coverage.get(target).cloned().unwrap_or_default();
            if covering.is_empty() {
                return (target.clone(), String::new());
            }

            // Strict completeness check: every covering node must have an
            // entry in `lean_relevant_descendants_per_covering`. The helper
            // omits entries on payload failure (lake hiccup), and a partial
            // map would produce a fingerprint with an incomplete
            // `lean_relevant_definition_descendants` axis that disagrees
            // with the next healthy observation, triggering spurious
            // re-verifies. Return empty fingerprint for the target instead.
            if !covering
                .iter()
                .all(|c| lean_relevant_descendants_per_covering.contains_key(c))
            {
                return (target.clone(), String::new());
            }

            let mut covering_nodes = BTreeMap::new();
            let mut lean_relevant_definition_descendants = BTreeMap::new();
            let mut import_closure = BTreeSet::new();
            for node in &covering {
                covering_nodes.insert(
                    node.clone(),
                    node_statement_hash(repo_path, node).unwrap_or_default(),
                );
                recursive_import_closure(repo_path, node, &mut import_closure);
                if let Some(l_def) = lean_relevant_descendants_per_covering.get(node) {
                    for dep in l_def {
                        if dep.as_str() == PREAMBLE_NAME || covering.contains(dep) {
                            continue;
                        }
                        lean_relevant_definition_descendants
                            .entry(dep.clone())
                            .or_insert_with(|| {
                                node_statement_hash(repo_path, dep.as_str()).unwrap_or_default()
                            });
                    }
                }
            }

            let imports_preamble = import_closure.contains(PREAMBLE_NAME);

            let approved_preamble_hashes = approved_paper_fingerprints
                .get(target)
                .and_then(|value| decode_paper_target_fingerprint(value))
                .map(|value| value.preamble_definition_hashes)
                .unwrap_or_default();
            let preamble_definition_hashes = if !imports_preamble {
                BTreeSet::new()
            } else if approved_preamble_hashes.is_empty() {
                current_preamble_hashes.clone()
            } else {
                current_preamble_hashes
                    .intersection(&approved_preamble_hashes)
                    .cloned()
                    .collect()
            };

            let fingerprint = PaperTargetFingerprint {
                target: target.clone(),
                covering_nodes,
                lean_relevant_definition_descendants,
                preamble_definition_hashes,
            };
            (
                target.clone(),
                serde_json::to_string(&fingerprint).unwrap_or_default(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::observe_paper_faithfulness_fingerprints;
    use crate::model::{NodeId, TargetId};
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;

    fn set<T: From<String> + Ord>(items: &[&str]) -> BTreeSet<T> {
        items
            .iter()
            .map(|item| T::from((*item).to_string()))
            .collect()
    }

    #[test]
    fn new_preamble_definition_does_not_reopen_without_prior_dependency() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tablet = tmp.path().join("Tablet");
        fs::create_dir_all(&tablet).expect("tablet dir");
        fs::write(
            tablet.join("Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        )
        .expect("write preamble lean");
        fs::write(
            tablet.join("Preamble.tex"),
            "\\begin{definition}[Old]\nold\n\\end{definition}\n",
        )
        .expect("write preamble tex");
        fs::write(
            tablet.join("Cover.lean"),
            "import Tablet.Preamble\n\n-- [TABLET NODE: Cover]\ntheorem Cover : True := by\n  trivial\n",
        )
        .expect("write cover lean");
        fs::write(
            tablet.join("Cover.tex"),
            "\\begin{theorem}\ncover\n\\end{theorem}\n\\begin{proof}\nproof\n\\end{proof}\n",
        )
        .expect("write cover tex");

        let configured_targets: BTreeSet<TargetId> = set(&["t"]);
        let target_claims: BTreeMap<NodeId, BTreeSet<TargetId>> =
            BTreeMap::from([(NodeId::from("Cover"), set(&["t"]))]);
        let present_nodes: BTreeSet<NodeId> = set(&["Preamble", "Cover"]);
        // Complete-empty L_def map: covering node `Cover` has no
        // Lean-relevant descendants. Distinct from BTreeMap::new() (which
        // signals "couldn't determine" → empty fingerprint) — `Cover→{}`
        // signals "complete: no relevant deps" → fingerprint with empty
        // descendant axis but populated covering/preamble axes.
        let l_def_complete: BTreeMap<NodeId, BTreeSet<NodeId>> =
            BTreeMap::from([(NodeId::from("Cover"), BTreeSet::new())]);

        let approved = observe_paper_faithfulness_fingerprints(
            tmp.path(),
            &configured_targets,
            &target_claims,
            &present_nodes,
            &BTreeMap::new(),
            &l_def_complete,
        );

        fs::write(
            tablet.join("Preamble.tex"),
            "\\begin{definition}[Old]\nold\n\\end{definition}\n\\begin{definition}[New]\nnew\n\\end{definition}\n",
        )
        .expect("rewrite preamble tex");

        let current = observe_paper_faithfulness_fingerprints(
            tmp.path(),
            &configured_targets,
            &target_claims,
            &present_nodes,
            &approved,
            &l_def_complete,
        );

        assert!(
            !approved.get("t").map(|s| s.is_empty()).unwrap_or(true),
            "test prerequisite: approved fingerprint must be non-empty"
        );
        assert_eq!(approved.get("t"), current.get("t"));
    }

    #[test]
    fn lean_relevant_descendant_tex_change_reopens_target_irrelevant_does_not() {
        // Defn is in Cover's textual import closure but only Lean-relevant
        // when L_def(Cover) includes it. With an EMPTY L_def map, editing
        // Defn.tex must NOT flip the paper fingerprint (Lean-irrelevant
        // change). With Cover -> {Defn} L_def, editing Defn.tex MUST flip
        // the fingerprint.
        let tmp = tempfile::tempdir().expect("tempdir");
        let tablet = tmp.path().join("Tablet");
        fs::create_dir_all(&tablet).expect("tablet dir");
        fs::write(tablet.join("Preamble.lean"), "").expect("write preamble lean");
        fs::write(tablet.join("Preamble.tex"), "").expect("write preamble tex");
        fs::write(
            tablet.join("Defn.lean"),
            "-- [TABLET NODE: Defn]\ndef Defn := 1\n",
        )
        .expect("write defn lean");
        fs::write(
            tablet.join("Defn.tex"),
            "\\begin{definition}\nold def\n\\end{definition}\n",
        )
        .expect("write defn tex");
        fs::write(
            tablet.join("Cover.lean"),
            "import Tablet.Defn\n\n-- [TABLET NODE: Cover]\ntheorem Cover : True := by\n  trivial\n",
        )
        .expect("write cover lean");
        fs::write(
            tablet.join("Cover.tex"),
            "\\begin{theorem}\ncover\n\\end{theorem}\n\\begin{proof}\nproof\n\\end{proof}\n",
        )
        .expect("write cover tex");

        let configured_targets: BTreeSet<TargetId> = set(&["t"]);
        let target_claims: BTreeMap<NodeId, BTreeSet<TargetId>> =
            BTreeMap::from([(NodeId::from("Cover"), set(&["t"]))]);
        let present_nodes: BTreeSet<NodeId> = set(&["Preamble", "Defn", "Cover"]);

        // L_def(Cover) = {Defn}: Defn is Lean-relevant for Cover.
        let l_def_with_defn: BTreeMap<NodeId, BTreeSet<NodeId>> =
            BTreeMap::from([(NodeId::from("Cover"), set::<NodeId>(&["Defn"]))]);
        // L_def(Cover) = {}: Defn is in Cover's textual import closure but
        // NOT in its Lean-semantic-closure walk. Complete map (covering
        // node has an entry) so the fingerprint is non-empty.
        let l_def_irrelevant: BTreeMap<NodeId, BTreeSet<NodeId>> =
            BTreeMap::from([(NodeId::from("Cover"), BTreeSet::new())]);

        let approved_relevant = observe_paper_faithfulness_fingerprints(
            tmp.path(),
            &configured_targets,
            &target_claims,
            &present_nodes,
            &BTreeMap::new(),
            &l_def_with_defn,
        );
        let approved_irrelevant = observe_paper_faithfulness_fingerprints(
            tmp.path(),
            &configured_targets,
            &target_claims,
            &present_nodes,
            &BTreeMap::new(),
            &l_def_irrelevant,
        );

        fs::write(
            tablet.join("Defn.tex"),
            "\\begin{definition}\nnew def\n\\end{definition}\n",
        )
        .expect("rewrite defn tex");

        let current_relevant = observe_paper_faithfulness_fingerprints(
            tmp.path(),
            &configured_targets,
            &target_claims,
            &present_nodes,
            &approved_relevant,
            &l_def_with_defn,
        );
        let current_irrelevant = observe_paper_faithfulness_fingerprints(
            tmp.path(),
            &configured_targets,
            &target_claims,
            &present_nodes,
            &approved_irrelevant,
            &l_def_irrelevant,
        );

        // Sanity: both starting fingerprints must be non-empty so the
        // assertions are meaningful.
        assert!(
            !approved_relevant
                .get("t")
                .map(|s| s.is_empty())
                .unwrap_or(true),
            "test prerequisite: relevant approved fingerprint must be non-empty"
        );
        assert!(
            !approved_irrelevant
                .get("t")
                .map(|s| s.is_empty())
                .unwrap_or(true),
            "test prerequisite: irrelevant approved fingerprint must be non-empty"
        );
        assert_ne!(
            approved_relevant.get("t"),
            current_relevant.get("t"),
            "Lean-relevant Defn TeX change must flip the paper fingerprint"
        );
        assert_eq!(
            approved_irrelevant.get("t"),
            current_irrelevant.get("t"),
            "Lean-irrelevant Defn TeX change must NOT flip the paper fingerprint"
        );
    }
}
