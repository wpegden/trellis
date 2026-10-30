//! Safe, extraction-free verification of the exact authorized ZIP payload.

use super::canonical::{
    canonical_json_value, parse_json_strict, verify_self_digest, DomainTag, Sha256Digest,
    TrustError,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};

pub const PACKAGE_MANIFEST_PATH: &str = "TRELLIS_PACKAGE_MANIFEST.json";
const MAX_ARCHIVE_ENTRIES: usize = 100_000;
const MAX_ENTRY_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedArchiveManifest {
    pub manifest_sha256: Sha256Digest,
    pub entry_count: usize,
    pub total_uncompressed_bytes: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestWire {
    schema: String,
    entries: Vec<ManifestEntryWire>,
    manifest_sha256: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntryWire {
    path: String,
    byte_length: u64,
    sha256_of_raw_bytes: Sha256Digest,
}

/// Validate names and file types before reading any entry body, then verify a
/// canonical complete manifest without extracting the archive.
pub fn verify_package_archive(bytes: &[u8]) -> Result<VerifiedArchiveManifest, TrustError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| {
        TrustError::new("package_zip_invalid", format!("cannot open ZIP: {error}"))
    })?;
    if archive.len() == 0 || archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(TrustError::new(
            "package_zip_entry_count_invalid",
            format!("ZIP has {} entries", archive.len()),
        ));
    }
    let mut names = BTreeSet::new();
    let mut file_sizes = BTreeMap::new();
    let mut total = 0_u64;
    for index in 0..archive.len() {
        let entry = archive.by_index_raw(index).map_err(|error| {
            TrustError::new("package_zip_entry_invalid", error.to_string())
        })?;
        let name = entry.name().to_owned();
        validate_archive_path(&name, entry.is_dir())?;
        if !names.insert(name.clone()) {
            return Err(TrustError::new(
                "package_zip_duplicate_name",
                format!("duplicate ZIP member {name:?}"),
            ));
        }
        if let Some(mode) = entry.unix_mode() {
            let file_type = mode & 0o170000;
            if file_type != 0 && file_type != 0o100000 && file_type != 0o040000 {
                return Err(TrustError::new(
                    "package_zip_special_file",
                    format!("ZIP member {name:?} is not a regular file or directory"),
                ));
            }
        }
        if !entry.is_dir() {
            if entry.size() > MAX_ENTRY_BYTES {
                return Err(TrustError::new(
                    "package_zip_entry_too_large",
                    format!("ZIP member {name:?} exceeds the v1 size limit"),
                ));
            }
            total = total.checked_add(entry.size()).ok_or_else(|| {
                TrustError::new("package_zip_size_overflow", "ZIP size sum overflow")
            })?;
            if total > MAX_TOTAL_BYTES {
                return Err(TrustError::new(
                    "package_zip_total_too_large",
                    "ZIP uncompressed size exceeds the v1 limit",
                ));
            }
            file_sizes.insert(name, entry.size());
        }
    }
    let manifest_index = archive.index_for_name(PACKAGE_MANIFEST_PATH).ok_or_else(|| {
        TrustError::new(
            "package_manifest_missing",
            format!("ZIP lacks root {PACKAGE_MANIFEST_PATH}"),
        )
    })?;
    let manifest_bytes = {
        let mut entry = archive.by_index(manifest_index).map_err(|error| {
            TrustError::new("package_manifest_unreadable", error.to_string())
        })?;
        if entry.size() > MAX_MANIFEST_BYTES {
            return Err(TrustError::new(
                "package_manifest_too_large",
                "package manifest exceeds the v1 size limit",
            ));
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes).map_err(|error| {
            TrustError::new("package_manifest_unreadable", error.to_string())
        })?;
        bytes
    };
    let manifest_value = parse_json_strict(&manifest_bytes)
        .map_err(|error| TrustError::new("package_manifest_json_invalid", error.to_string()))?;
    if canonical_json_value(&manifest_value)? != manifest_bytes {
        return Err(TrustError::new(
            "package_manifest_not_canonical",
            "package manifest must be exact canonical JSON",
        ));
    }
    let manifest_digest = verify_self_digest(
        DomainTag::ManifestNode,
        &manifest_value,
        "manifest_sha256",
    )?;
    let manifest: ManifestWire = serde_json::from_value(manifest_value)
        .map_err(|error| TrustError::new("package_manifest_decode_failed", error.to_string()))?;
    if manifest.schema != "trellis-package-manifest/v1"
        || manifest.manifest_sha256 != manifest_digest
    {
        return Err(TrustError::new(
            "package_manifest_identity_mismatch",
            "package manifest schema or digest is invalid",
        ));
    }
    let mut prior: Option<&str> = None;
    let mut declared = BTreeSet::new();
    for item in &manifest.entries {
        validate_archive_path(&item.path, false)?;
        if item.path == PACKAGE_MANIFEST_PATH {
            return Err(TrustError::new(
                "package_manifest_self_entry_forbidden",
                "package manifest excludes itself to avoid a hash cycle",
            ));
        }
        if prior.is_some_and(|previous| previous.as_bytes() >= item.path.as_bytes())
            || !declared.insert(item.path.clone())
        {
            return Err(TrustError::new(
                "package_manifest_order_or_duplicate",
                "manifest entries must be unique and strictly UTF-8 byte sorted",
            ));
        }
        prior = Some(&item.path);
        let actual_size = file_sizes.get(&item.path).ok_or_else(|| {
            TrustError::new(
                "package_manifest_member_missing",
                format!("manifest names missing file {:?}", item.path),
            )
        })?;
        if *actual_size != item.byte_length {
            return Err(TrustError::new(
                "package_manifest_size_mismatch",
                format!("manifest size differs for {:?}", item.path),
            ));
        }
        let entry = archive.by_name(&item.path).map_err(|error| {
            TrustError::new("package_member_unreadable", error.to_string())
        })?;
        let (actual_digest, bytes_read) = hash_reader(entry)?;
        if bytes_read != item.byte_length {
            return Err(TrustError::new(
                "package_member_stream_size_mismatch",
                format!("decompressed byte count differs for {:?}", item.path),
            ));
        }
        if actual_digest != item.sha256_of_raw_bytes {
            return Err(TrustError::new(
                "package_manifest_digest_mismatch",
                format!("manifest digest differs for {:?}", item.path),
            ));
        }
    }
    let actual_files: BTreeSet<_> = file_sizes
        .keys()
        .filter(|path| path.as_str() != PACKAGE_MANIFEST_PATH)
        .cloned()
        .collect();
    if declared != actual_files {
        return Err(TrustError::new(
            "package_manifest_not_complete",
            "ZIP contains an unmanifested file or omits a file",
        ));
    }
    Ok(VerifiedArchiveManifest {
        manifest_sha256: manifest_digest,
        entry_count: declared.len(),
        total_uncompressed_bytes: total,
    })
}

/// Enforce the v1 external-claim presentation boundary.  The package may
/// contain arbitrary machine inputs and proof artifacts, but it may expose
/// only the kernel-generated claim document as prose.  Without this check a
/// completely manifested `README.md` could make a stronger claim than the
/// authenticated four-line rows while leaving every cryptographic check
/// green.
///
/// This is deliberately separate from [`verify_package_archive`]: the latter
/// is the generic safe-ZIP verifier, while this is package-authorization
/// policy.  Callers must run the generic verifier first.
pub(crate) fn verify_single_claim_presentation(
    bytes: &[u8],
    approved_claim_path: &str,
) -> Result<(), TrustError> {
    validate_archive_path(approved_claim_path, false)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| {
        TrustError::new("package_zip_invalid", format!("cannot open ZIP: {error}"))
    })?;
    for index in 0..archive.len() {
        let entry = archive.by_index_raw(index).map_err(|error| {
            TrustError::new("package_zip_entry_invalid", error.to_string())
        })?;
        if entry.is_dir() || entry.name() == approved_claim_path {
            continue;
        }
        if is_prose_presentation_path(entry.name()) {
            return Err(TrustError::new(
                "package_unapproved_claim_presentation",
                format!(
                    "prose member {:?} is not the sole approved claim surface {approved_claim_path:?}",
                    entry.name()
                ),
            ));
        }
    }
    Ok(())
}

fn is_prose_presentation_path(path: &str) -> bool {
    let basename = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    if basename == "readme" || basename.starts_with("readme.") {
        return true;
    }
    [
        ".md",
        ".markdown",
        ".txt",
        ".rst",
        ".adoc",
        ".html",
        ".htm",
    ]
    .iter()
    .any(|extension| basename.ends_with(extension))
}

/// Read one small member only after callers have verified the complete
/// archive. The exact-name lookup and explicit bound keep package metadata
/// parsing extraction-free.
pub(crate) fn read_package_member(
    bytes: &[u8],
    path: &str,
    maximum_bytes: u64,
) -> Result<Vec<u8>, TrustError> {
    validate_archive_path(path, false)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| {
        TrustError::new("package_zip_invalid", format!("cannot open ZIP: {error}"))
    })?;
    let mut entry = archive.by_name(path).map_err(|_| {
        TrustError::new(
            "package_required_member_missing",
            format!("ZIP lacks required member {path}"),
        )
    })?;
    if entry.size() > maximum_bytes {
        return Err(TrustError::new(
            "package_required_member_too_large",
            format!("required member {path} exceeds its size limit"),
        ));
    }
    let mut output = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut output).map_err(|error| {
        TrustError::new("package_member_unreadable", error.to_string())
    })?;
    Ok(output)
}

fn hash_reader(mut reader: impl Read) -> Result<(Sha256Digest, u64), TrustError> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    let mut total = 0_u64;
    loop {
        let count = reader.read(&mut buffer).map_err(|error| {
            TrustError::new("package_member_unreadable", error.to_string())
        })?;
        if count == 0 {
            break;
        }
        total = total.checked_add(count as u64).ok_or_else(|| {
            TrustError::new("package_member_size_overflow", "member byte count overflow")
        })?;
        if total > MAX_ENTRY_BYTES {
            return Err(TrustError::new(
                "package_zip_entry_too_large",
                "decompressed member exceeds the v1 size limit",
            ));
        }
        hasher.update(&buffer[..count]);
    }
    Ok((
        Sha256Digest::from_bytes(hasher.finalize().into()),
        total,
    ))
}

fn validate_archive_path(path: &str, directory: bool) -> Result<(), TrustError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path.chars().any(char::is_control)
    {
        return Err(TrustError::new(
            "package_zip_unsafe_path",
            format!("unsafe ZIP member path {path:?}"),
        ));
    }
    let trimmed = if directory {
        path.strip_suffix('/').unwrap_or(path)
    } else {
        path
    };
    if trimmed.is_empty()
        || trimmed
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
        || (!directory && path.ends_with('/'))
    {
        return Err(TrustError::new(
            "package_zip_unsafe_path",
            format!("non-normalized ZIP member path {path:?}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust_base::{canonical_json_value, raw_sha256, self_digest};
    use serde_json::Value;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    fn archive(entries: &[(&str, &[u8])], manifest_paths: &[&str]) -> Vec<u8> {
        let manifest_entries: Vec<_> = manifest_paths
            .iter()
            .map(|path| {
                let bytes = entries
                    .iter()
                    .find(|(candidate, _)| candidate == path)
                    .unwrap()
                    .1;
                serde_json::json!({
                    "path": path,
                    "byte_length": bytes.len(),
                    "sha256_of_raw_bytes": raw_sha256(bytes),
                })
            })
            .collect();
        let mut manifest = serde_json::json!({
            "schema": "trellis-package-manifest/v1",
            "entries": manifest_entries,
            "manifest_sha256": Sha256Digest::ZERO,
        });
        let digest = self_digest(DomainTag::ManifestNode, &manifest, "manifest_sha256").unwrap();
        manifest["manifest_sha256"] = Value::String(digest.to_string());
        let manifest = canonical_json_value(&manifest).unwrap();
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            let options = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .unix_permissions(0o100644);
            for (path, bytes) in entries {
                writer.start_file(*path, options).unwrap();
                writer.write_all(bytes).unwrap();
            }
            writer.start_file(PACKAGE_MANIFEST_PATH, options).unwrap();
            writer.write_all(&manifest).unwrap();
            writer.finish().unwrap();
        }
        cursor.into_inner()
    }

    #[test]
    fn valid_archive_is_verified_without_extraction() {
        let bytes = archive(&[("README.md", b"ok\n")], &["README.md"]);
        let verified = verify_package_archive(&bytes).unwrap();
        assert_eq!(verified.entry_count, 1);
    }

    #[test]
    fn unsafe_paths_fail_before_manifest_trust() {
        let unsafe_zip = archive(&[("../escape", b"x")], &["../escape"]);
        assert_eq!(
            verify_package_archive(&unsafe_zip).unwrap_err().code,
            "package_zip_unsafe_path"
        );
    }

    #[test]
    fn unmanifested_file_is_rejected() {
        let bytes = archive(&[("a", b"a"), ("extra", b"x")], &["a"]);
        assert_eq!(
            verify_package_archive(&bytes).unwrap_err().code,
            "package_manifest_not_complete"
        );
    }

    #[test]
    fn authorization_presentation_policy_rejects_a_second_prose_surface() {
        let bytes = archive(
            &[
                ("WHAT_THE_PROOFS_ASSUME.md", b"four exact rows\n"),
                ("docs/README.md", b"verified in practice\n"),
            ],
            &["WHAT_THE_PROOFS_ASSUME.md", "docs/README.md"],
        );
        verify_package_archive(&bytes).unwrap();
        assert_eq!(
            verify_single_claim_presentation(&bytes, "WHAT_THE_PROOFS_ASSUME.md")
                .unwrap_err()
                .code,
            "package_unapproved_claim_presentation"
        );
    }

    #[test]
    fn authorization_presentation_policy_allows_machine_artifacts() {
        let bytes = archive(
            &[
                ("WHAT_THE_PROOFS_ASSUME.md", b"four exact rows\n"),
                ("artifacts/proof.bin", b"proof bytes"),
                ("records/result.json", b"{}"),
            ],
            &[
                "WHAT_THE_PROOFS_ASSUME.md",
                "artifacts/proof.bin",
                "records/result.json",
            ],
        );
        verify_package_archive(&bytes).unwrap();
        verify_single_claim_presentation(&bytes, "WHAT_THE_PROOFS_ASSUME.md").unwrap();
    }
}
