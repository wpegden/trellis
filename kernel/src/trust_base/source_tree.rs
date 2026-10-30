//! Canonical regular-file source-tree manifest shared by bootstrap and Phase 0.

use super::canonical::{
    canonical_json, canonical_json_value, raw_sha256, tagged_hash, DomainTag, Sha256Digest,
    TrustError,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

pub const SOURCE_TREE_MANIFEST_SCHEMA: &str = "trellis-source-tree-manifest/v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTreeEntry {
    pub relative_path: String,
    pub byte_length: usize,
    pub sha256_of_raw_bytes: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTreeManifest {
    pub schema: String,
    pub files: Vec<SourceTreeEntry>,
    pub source_tree_sha256: Sha256Digest,
}

/// Read a source tree without following links. Empty directories are not source
/// semantics; an entirely empty regular-file tree is rejected.
pub fn read_source_tree(root: &Path) -> Result<BTreeMap<String, Vec<u8>>, TrustError> {
    fn walk(
        root: &Path,
        current: &Path,
        output: &mut BTreeMap<String, Vec<u8>>,
    ) -> Result<(), TrustError> {
        let mut entries = fs::read_dir(current)
            .map_err(|error| tree_error("source_tree_unreadable", current, error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| tree_error("source_tree_unreadable", current, error))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| tree_error("source_tree_unreadable", &path, error))?;
            if metadata.file_type().is_symlink() {
                return Err(TrustError::new(
                    "source_tree_symlink_forbidden",
                    format!("{} is a symlink", path.display()),
                ));
            }
            if metadata.is_dir() {
                walk(root, &path, output)?;
                continue;
            }
            if !metadata.is_file() {
                return Err(TrustError::new(
                    "source_tree_special_file",
                    format!("{} is not a regular file or directory", path.display()),
                ));
            }
            let relative = path.strip_prefix(root).map_err(|_| {
                TrustError::new("source_tree_escape", "tree member escapes source root")
            })?;
            let relative = normalized_relative_path(relative)?;
            let bytes = read_same_regular_file(&path, &metadata)?;
            if output.insert(relative.clone(), bytes).is_some() {
                return Err(TrustError::new(
                    "source_tree_duplicate_path",
                    format!("source tree repeats {relative}"),
                ));
            }
        }
        Ok(())
    }

    let metadata = fs::symlink_metadata(root)
        .map_err(|error| tree_error("source_tree_missing", root, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(TrustError::new(
            "source_tree_root_invalid",
            format!("{} is not a non-symlink directory", root.display()),
        ));
    }
    let mut output = BTreeMap::new();
    walk(root, root, &mut output)?;
    if output.is_empty() {
        return Err(TrustError::new(
            "source_tree_empty",
            "source tree contains no regular files",
        ));
    }
    Ok(output)
}

fn read_same_regular_file(path: &Path, expected: &fs::Metadata) -> Result<Vec<u8>, TrustError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options
        .open(path)
        .map_err(|error| tree_error("source_tree_unreadable", path, error))?;
    let opened = file
        .metadata()
        .map_err(|error| tree_error("source_tree_unreadable", path, error))?;
    if !opened.is_file() {
        return Err(TrustError::new(
            "source_tree_file_changed",
            format!("{} is no longer a regular file", path.display()),
        ));
    }
    #[cfg(unix)]
    if opened.dev() != expected.dev() || opened.ino() != expected.ino() {
        return Err(TrustError::new(
            "source_tree_file_changed",
            format!("{} changed identity while being read", path.display()),
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| tree_error("source_tree_unreadable", path, error))?;
    if u64::try_from(bytes.len()).ok() != Some(opened.len()) {
        return Err(TrustError::new(
            "source_tree_file_changed",
            format!("{} changed length while being read", path.display()),
        ));
    }
    Ok(bytes)
}

pub fn source_tree_manifest(root: &Path) -> Result<SourceTreeManifest, TrustError> {
    source_tree_manifest_from_files(&read_source_tree(root)?)
}

pub fn source_tree_manifest_from_files(
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<SourceTreeManifest, TrustError> {
    if files.is_empty() {
        return Err(TrustError::new(
            "source_tree_empty",
            "source tree contains no regular files",
        ));
    }
    let entries = files
        .iter()
        .map(|(relative_path, bytes)| {
            validate_relative_path(relative_path)?;
            Ok(SourceTreeEntry {
                relative_path: relative_path.clone(),
                byte_length: bytes.len(),
                sha256_of_raw_bytes: raw_sha256(bytes),
            })
        })
        .collect::<Result<Vec<_>, TrustError>>()?;
    let source_tree_sha256 = source_tree_root_from_entries(&entries)?;
    Ok(SourceTreeManifest {
        schema: SOURCE_TREE_MANIFEST_SCHEMA.to_owned(),
        files: entries,
        source_tree_sha256,
    })
}

pub fn source_tree_root_from_entries(
    entries: &[SourceTreeEntry],
) -> Result<Sha256Digest, TrustError> {
    if entries.is_empty() {
        return Err(TrustError::new(
            "source_tree_empty",
            "source tree contains no regular files",
        ));
    }
    let mut previous: Option<&str> = None;
    for entry in entries {
        validate_relative_path(&entry.relative_path)?;
        if entry.sha256_of_raw_bytes == Sha256Digest::ZERO {
            return Err(TrustError::new(
                "source_tree_entry_invalid",
                "source tree entry has a zero digest",
            ));
        }
        if previous.is_some_and(|value| value.as_bytes() >= entry.relative_path.as_bytes()) {
            return Err(TrustError::new(
                "source_tree_manifest_not_canonical",
                "source tree entries must be strictly sorted by UTF-8 path bytes",
            ));
        }
        previous = Some(&entry.relative_path);
    }
    let value = serde_json::to_value(entries).map_err(|error| {
        TrustError::new("source_tree_manifest_invalid", error.to_string())
    })?;
    Ok(tagged_hash(
        DomainTag::ManifestNode,
        &canonical_json_value(&value)?,
    ))
}

pub fn source_tree_digest(root: &Path) -> Result<Sha256Digest, TrustError> {
    Ok(source_tree_manifest(root)?.source_tree_sha256)
}

pub fn canonical_source_tree_manifest(root: &Path) -> Result<Vec<u8>, TrustError> {
    canonical_json(&source_tree_manifest(root)?)
}

pub fn source_tree_manifest_value(root: &Path) -> Result<Value, TrustError> {
    serde_json::to_value(source_tree_manifest(root)?).map_err(|error| {
        TrustError::new("source_tree_manifest_invalid", error.to_string())
    })
}

pub fn validate_relative_path(value: &str) -> Result<(), TrustError> {
    let path = PathBuf::from(value);
    if value.is_empty()
        || value.contains('\\')
        || value.contains('\0')
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(TrustError::new(
            "source_tree_relative_path_invalid",
            format!("invalid relative source path {value:?}"),
        ));
    }
    Ok(())
}

fn normalized_relative_path(path: &Path) -> Result<String, TrustError> {
    let mut pieces = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(piece) = component else {
            return Err(TrustError::new(
                "source_tree_relative_path_invalid",
                format!("{} is not normalized", path.display()),
            ));
        };
        pieces.push(piece.to_str().ok_or_else(|| {
            TrustError::new("source_tree_path_not_utf8", "source tree path is not UTF-8")
        })?);
    }
    let value = pieces.join("/");
    validate_relative_path(&value)?;
    Ok(value)
}

fn tree_error(code: &'static str, path: &Path, error: std::io::Error) -> TrustError {
    TrustError::new(code, format!("{}: {error}", path.display()))
}
