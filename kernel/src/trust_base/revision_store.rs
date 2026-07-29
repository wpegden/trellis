//! Immutable, content-addressed storage for exceptional trust-basis revisions.
//!
//! The journal records closure digests, not mutable filesystem paths.  This
//! module gives those digests one deterministic location beneath the external
//! journal root.  A protected reapproval can therefore be recovered after a
//! restart without repointing the runtime's initial seed paths or overwriting
//! the initial closure.

use super::canonical::{
    canonical_json_value, parse_json_strict, raw_sha256, tagged_hash, DomainTag, Sha256Digest,
    TrustError,
};
use super::closure::{
    verify_evidence_tool_manifest, verify_seed_definition_bundle, VerifiedEvidenceClosure,
    VerifiedSeedDefinitionClosure,
};
use super::journal::RevisionClosureProjection;
use super::records::AuthoritativeRecord;
use super::schema::SchemaRegistry;
use super::seed::validate_seed_manifest_semantics;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const STORE_DIR: &str = "revision-closures-v1";
const MARKER_SCHEMA: &str = "trellis-revision-closure-store-entry/v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionClosurePaths {
    pub address: Sha256Digest,
    pub root: PathBuf,
    pub seed_manifest_path: PathBuf,
    pub seed_definition_bundle_path: PathBuf,
    pub evidence_manifest_path: PathBuf,
    pub evidence_root_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ResolvedRevisionClosure {
    pub paths: RevisionClosurePaths,
    pub seed: VerifiedSeedDefinitionClosure,
    pub evidence: VerifiedEvidenceClosure,
}

/// Compute the stable store address for the exact journal projection.
pub fn revision_closure_address(
    closure: &RevisionClosureProjection,
) -> Result<Sha256Digest, TrustError> {
    Ok(tagged_hash(
        DomainTag::ManifestNode,
        &canonical_json_value(&serde_json::json!({
            "schema": "trellis-revision-closure-address/v1",
            "seed_manifest_sha256": closure.seed_manifest_sha256,
            "seed_definition_bundle_sha256": closure.seed_definition_bundle_sha256,
            "authored_semantic_root": closure.authored_semantic_root,
            "evidence_manifest_sha256": closure.evidence_manifest_sha256,
            "evidence_tool_input_root": closure.evidence_tool_input_root,
        }))?,
    ))
}

pub fn revision_closure_paths(
    journal_root: &Path,
    closure: &RevisionClosureProjection,
) -> Result<RevisionClosurePaths, TrustError> {
    let address = revision_closure_address(closure)?;
    let root = journal_root.join(STORE_DIR).join(address.to_string());
    Ok(RevisionClosurePaths {
        address,
        seed_manifest_path: root.join("SEED_MANIFEST.json"),
        seed_definition_bundle_path: root.join("SEED_DEFINITION_BUNDLE.json"),
        evidence_manifest_path: root.join("EVIDENCE_TOOL_MANIFEST.json"),
        evidence_root_path: root.join("evidence-root"),
        root,
    })
}

/// Publish a fully verified revision closure before its `revision_opened`
/// journal event is committed.  Publication is an atomic, no-overwrite rename.
/// A pre-existing address is accepted only after complete byte/digest
/// revalidation, which makes retries idempotent without making the store
/// mutable.
pub fn publish_revision_closure(
    journal_root: &Path,
    seed_manifest_bytes: &[u8],
    seed_definition_bundle_bytes: &[u8],
    evidence_root: &Path,
    evidence_manifest_bytes: &[u8],
    expected: &RevisionClosureProjection,
) -> Result<ResolvedRevisionClosure, TrustError> {
    verify_supplied_closure(
        seed_manifest_bytes,
        seed_definition_bundle_bytes,
        evidence_root,
        evidence_manifest_bytes,
        expected,
    )?;
    let journal_root = canonical_directory(journal_root, "journal root")?;
    let store = journal_root.join(STORE_DIR);
    ensure_plain_directory(&store)?;
    let paths = revision_closure_paths(&journal_root, expected)?;
    if path_exists(&paths.root)? {
        return load_revision_closure(&journal_root, expected);
    }

    let staging = allocate_staging_directory(&store)?;
    let result = (|| {
        write_new_synced(&staging.join("SEED_MANIFEST.json"), seed_manifest_bytes, false)?;
        write_new_synced(
            &staging.join("SEED_DEFINITION_BUNDLE.json"),
            seed_definition_bundle_bytes,
            false,
        )?;
        write_new_synced(
            &staging.join("EVIDENCE_TOOL_MANIFEST.json"),
            evidence_manifest_bytes,
            false,
        )?;
        let staged_evidence = staging.join("evidence-root");
        fs::create_dir(&staged_evidence).map_err(io_error("create staged evidence root"))?;
        let verified_evidence = verify_evidence_tool_manifest(evidence_root, evidence_manifest_bytes)?;
        for (logical_id, leaf) in &verified_evidence.leaves_by_logical_id {
            let source = verified_evidence.resolve_leaf_path(evidence_root, logical_id)?;
            let destination = staged_evidence.join(&leaf.relative_path);
            if let Some(parent) = destination.parent() {
                create_plain_directories_beneath(&staged_evidence, parent)?;
            }
            let source_metadata = fs::metadata(&source).map_err(io_error("stat evidence leaf"))?;
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                source_metadata.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            let bytes = fs::read(&source).map_err(io_error("read evidence leaf"))?;
            if bytes.len() as u64 != leaf.byte_length || raw_sha256(&bytes) != leaf.raw_sha256 {
                return Err(TrustError::new(
                    "revision_evidence_changed_during_copy",
                    format!("evidence leaf {logical_id} changed while publishing the revision"),
                ));
            }
            write_new_synced(&destination, &bytes, executable)?;
        }
        let marker = marker_bytes(expected, paths.address)?;
        write_new_synced(&staging.join("CLOSURE.json"), &marker, false)?;
        sync_directory_tree(&staging)?;
        match rename_noreplace(&staging, &paths.root) {
            Ok(()) => sync_directory(&store)?,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                fs::remove_dir_all(&staging).map_err(io_error("remove redundant staging closure"))?;
            }
            Err(error) => return Err(io_error("publish revision closure")(error)),
        }
        load_revision_closure(&journal_root, expected)
    })();
    if result.is_err() && path_exists(&staging).unwrap_or(false) {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

/// Resolve and revalidate the exact closure selected by the authoritative
/// journal.  No directory scan or "latest" pointer participates in lookup.
pub fn load_revision_closure(
    journal_root: &Path,
    expected: &RevisionClosureProjection,
) -> Result<ResolvedRevisionClosure, TrustError> {
    let journal_root = canonical_directory(journal_root, "journal root")?;
    let paths = revision_closure_paths(&journal_root, expected)?;
    let root = canonical_directory(&paths.root, "revision closure")?;
    if root != paths.root {
        return Err(TrustError::new(
            "revision_closure_path_redirected",
            "content-addressed revision closure path resolves elsewhere",
        ));
    }
    let marker = read_plain_file(&paths.root, "CLOSURE.json")?;
    if marker != marker_bytes(expected, paths.address)? {
        return Err(TrustError::new(
            "revision_closure_marker_mismatch",
            "stored revision marker differs from the journal-selected closure",
        ));
    }
    let seed_manifest_bytes = read_plain_file(&paths.root, "SEED_MANIFEST.json")?;
    let seed_bundle_bytes = read_plain_file(&paths.root, "SEED_DEFINITION_BUNDLE.json")?;
    let evidence_manifest_bytes = read_plain_file(&paths.root, "EVIDENCE_TOOL_MANIFEST.json")?;
    let (seed, evidence) = verify_supplied_closure(
        &seed_manifest_bytes,
        &seed_bundle_bytes,
        &paths.evidence_root_path,
        &evidence_manifest_bytes,
        expected,
    )?;
    Ok(ResolvedRevisionClosure { paths, seed, evidence })
}

fn verify_supplied_closure(
    seed_manifest_bytes: &[u8],
    seed_definition_bundle_bytes: &[u8],
    evidence_root: &Path,
    evidence_manifest_bytes: &[u8],
    expected: &RevisionClosureProjection,
) -> Result<(VerifiedSeedDefinitionClosure, VerifiedEvidenceClosure), TrustError> {
    let seed_value = parse_json_strict(seed_manifest_bytes)
        .map_err(|error| TrustError::new("revision_seed_json_invalid", error.to_string()))?;
    if canonical_json_value(&seed_value)? != seed_manifest_bytes {
        return Err(TrustError::new(
            "revision_seed_not_canonical",
            "revision seed manifest must be exact canonical JSON",
        ));
    }
    let seed = AuthoritativeRecord::parse(&SchemaRegistry::v1()?, seed_value)?;
    let verified_seed = verify_seed_definition_bundle(&seed, seed_definition_bundle_bytes)?;
    let roots = validate_seed_manifest_semantics(&seed)?;
    let verified_evidence = verify_evidence_tool_manifest(evidence_root, evidence_manifest_bytes)?;
    if verified_seed.seed_manifest_sha256 != expected.seed_manifest_sha256
        || verified_seed.bundle_sha256 != expected.seed_definition_bundle_sha256
        || roots.authored_semantic_root != expected.authored_semantic_root
        || verified_evidence.manifest_sha256 != expected.evidence_manifest_sha256
        || verified_evidence.evidence_tool_input_root != expected.evidence_tool_input_root
        || roots.evidence_tool_input_root != expected.evidence_tool_input_root
    {
        return Err(TrustError::new(
            "revision_closure_digest_mismatch",
            "revision closure bytes differ from the expected journal projection",
        ));
    }
    Ok((verified_seed, verified_evidence))
}

fn marker_bytes(
    closure: &RevisionClosureProjection,
    address: Sha256Digest,
) -> Result<Vec<u8>, TrustError> {
    canonical_json_value(&serde_json::json!({
        "schema": MARKER_SCHEMA,
        "closure_address_sha256": address,
        "seed_manifest_sha256": closure.seed_manifest_sha256,
        "seed_definition_bundle_sha256": closure.seed_definition_bundle_sha256,
        "authored_semantic_root": closure.authored_semantic_root,
        "evidence_manifest_sha256": closure.evidence_manifest_sha256,
        "evidence_tool_input_root": closure.evidence_tool_input_root,
    }))
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, TrustError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error("stat directory"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(TrustError::new(
            "revision_store_path_invalid",
            format!("{label} must be a non-symlink directory: {}", path.display()),
        ));
    }
    fs::canonicalize(path).map_err(io_error("resolve directory"))
}

fn ensure_plain_directory(path: &Path) -> Result<(), TrustError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(TrustError::new(
            "revision_store_path_invalid",
            format!("{} is not a plain directory", path.display()),
        )),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir(path).map_err(io_error("create revision closure store"))?;
            sync_directory(path.parent().ok_or_else(|| {
                TrustError::new("revision_store_path_invalid", "store path has no parent")
            })?)
        }
        Err(error) => Err(io_error("stat revision closure store")(error)),
    }
}

fn create_plain_directories_beneath(root: &Path, path: &Path) -> Result<(), TrustError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        TrustError::new("revision_store_path_invalid", "evidence destination escapes staging root")
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(TrustError::new(
                "revision_store_path_invalid",
                "evidence destination has an unsafe component",
            ));
        };
        current.push(component);
        ensure_plain_directory(&current)?;
    }
    Ok(())
}

fn allocate_staging_directory(parent: &Path) -> Result<PathBuf, TrustError> {
    for attempt in 0..128_u32 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = parent.join(format!(
            ".revision-closure-{}-{nanos}-{attempt}.staging",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error("create revision closure staging directory")(error)),
        }
    }
    Err(TrustError::new(
        "revision_store_staging_unavailable",
        "cannot allocate a unique revision closure staging directory",
    ))
}

fn write_new_synced(path: &Path, bytes: &[u8], executable: bool) -> Result<(), TrustError> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o555 } else { 0o444 });
    }
    let mut file = options.open(path).map_err(io_error("create revision closure member"))?;
    file.write_all(bytes).map_err(io_error("write revision closure member"))?;
    file.sync_all().map_err(io_error("fsync revision closure member"))?;
    Ok(())
}

fn read_plain_file(root: &Path, name: &str) -> Result<Vec<u8>, TrustError> {
    let path = root.join(name);
    let metadata = fs::symlink_metadata(&path).map_err(io_error("stat revision closure member"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(TrustError::new(
            "revision_closure_member_invalid",
            format!("{} is not a non-symlink regular file", path.display()),
        ));
    }
    fs::read(&path).map_err(io_error("read revision closure member"))
}

fn sync_directory_tree(root: &Path) -> Result<(), TrustError> {
    for entry in fs::read_dir(root).map_err(io_error("read revision closure staging tree"))? {
        let entry = entry.map_err(io_error("read revision closure staging entry"))?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(io_error("stat staged member"))?;
        if metadata.file_type().is_symlink() {
            return Err(TrustError::new(
                "revision_closure_symlink_forbidden",
                format!("{} is a symlink", entry.path().display()),
            ));
        }
        if metadata.is_dir() {
            sync_directory_tree(&entry.path())?;
        }
    }
    sync_directory(root)
}

#[cfg(target_os = "linux")]
fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "source path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "destination path contains NUL"))?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(destination).is_ok() {
        return Err(std::io::Error::new(ErrorKind::AlreadyExists, "destination exists"));
    }
    fs::rename(source, destination)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), TrustError> {
    fs::File::open(path)
        .map_err(io_error("open directory for fsync"))?
        .sync_all()
        .map_err(io_error("fsync directory"))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), TrustError> { Ok(()) }

fn path_exists(path: &Path) -> Result<bool, TrustError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("inspect revision closure path")(error)),
    }
}

fn io_error(context: &'static str) -> impl FnOnce(std::io::Error) -> TrustError {
    move |error| TrustError::new("revision_store_io", format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_binds_every_projection_field() {
        let digest = |byte: &str| byte.repeat(32).parse::<Sha256Digest>().unwrap();
        let base = RevisionClosureProjection {
            seed_manifest_sha256: digest("01"),
            seed_definition_bundle_sha256: digest("02"),
            authored_semantic_root: digest("03"),
            evidence_manifest_sha256: digest("04"),
            evidence_tool_input_root: digest("05"),
        };
        let address = revision_closure_address(&base).unwrap();
        for index in 0..5 {
            let mut changed = base;
            match index {
                0 => changed.seed_manifest_sha256 = digest("09"),
                1 => changed.seed_definition_bundle_sha256 = digest("09"),
                2 => changed.authored_semantic_root = digest("09"),
                3 => changed.evidence_manifest_sha256 = digest("09"),
                _ => changed.evidence_tool_input_root = digest("09"),
            }
            assert_ne!(address, revision_closure_address(&changed).unwrap());
        }
    }
}
