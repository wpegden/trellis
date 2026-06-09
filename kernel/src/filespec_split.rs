//! Text-based statement/proof boundary extractor for ordinary Tablet
//! `.lean` files.
//!
//! Replaces the parser-based `lean_decl_split` (Rust + Lean script,
//! deleted 2026-05-12). The Lean parser path repeatedly produced false
//! `parse_error` failures in production — most recently on Mathlib's
//! `scoped prefix:arg "#" => Finset.card`, where the parser state
//! didn't include the file's own `open Finset`. Once that bug class is
//! in play any future scoped-notation introduction is a latent burn-loop
//! trigger.
//!
//! The FILESPEC fix is dead simple: every ordinary `Tablet/<Node>.lean`
//! must contain exactly one line whose trimmed content is `-- BODY`.
//! That line is the statement/proof boundary marker, by FILESPEC fiat.
//! Lean treats it as a line comment, so the marker has zero interaction
//! with Lean's signature parser or indentation rules.
//!
//! There is **no declaration parsing** in this module. We do not look
//! for `theorem` / `def` keywords; we do not look for `:=` tokens; we
//! do not care about modifiers (`noncomputable`, `private`, `@[...]`),
//! command wrappers (`set_option … in <decl>`, `open … in <decl>`),
//! multi-line signatures, default arguments, or nested `let X := Y`.
//! Those are the bug classes the old parser path produced; the marker
//! line obviates all of them.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::NodeId;

/// Schema version of the in-memory split record.
pub const RECORD_VERSION: u32 = 1;

/// Trimmed content that identifies the statement/proof boundary
/// marker line. Single source of truth for the FILESPEC rule and the
/// splitter.
pub const BODY_MARKER_TRIMMED: &str = "-- BODY";

/// Trimmed prefix that identifies the tablet-node marker comment line
/// (the FILESPEC-required `-- [TABLET NODE: <Name>]`). Used as the
/// upper bound of the imports/opens preamble and the lower bound of
/// the protected declaration-signature region for `declaration_hash_strict`.
const TABLET_NODE_MARKER_PREFIX: &str = "-- [TABLET NODE:";

/// Namespace prefixes stripped from the normalised statement text
/// before hashing. Mirrors the legacy `normalize_declaration` in
/// `worker_normalization.rs` — any divergence here would create false
/// positives on declaration-signature drift checks. Update both sites
/// together.
const NAMESPACE_PREFIXES: &[&str] = &[
    "Filter.",
    "Real.",
    "Nat.",
    "Int.",
    "Set.",
    "Finset.",
    "MeasureTheory.",
    "Topology.",
    "ENNReal.",
    "NNReal.",
];

/// Splitter result. The slice
/// `content[..body_marker_start_byte]` is the entire statement region
/// (imports, opens, the principal declaration's signature, the
/// declaration-line `:=` token, etc. — everything before the marker
/// line). The slice `content[body_marker_end_byte..]` is the proof
/// body region.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilespecSplit {
    /// Node name (matches the file basename minus `.lean`).
    pub node: NodeId,
    /// UTF-8 byte offset of the FIRST byte of the `-- BODY` marker
    /// line.
    pub body_marker_start_byte: usize,
    /// UTF-8 byte offset of the byte AFTER the newline terminating the
    /// `-- BODY` marker line.
    pub body_marker_end_byte: usize,
    /// Lowercase hex SHA-256 of the entire file content.
    pub source_hash: String,
    /// Schema version of this record (see [`RECORD_VERSION`]).
    pub record_version: u32,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Read a tablet node's `.lean` file from `<repo>/Tablet/<node>.lean`.
/// Returns `(content, sha256_hex(content))`.
pub fn read_node_file(repo_path: &Path, node: &str) -> Result<(String, String), String> {
    let path = repo_path.join("Tablet").join(format!("{node}.lean"));
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("filespec_split: read {} failed: {}", path.display(), e))?;
    let source_hash = sha256_hex(content.as_bytes());
    Ok((content, source_hash))
}

/// Locate the unique `-- BODY` marker line. Returns
/// `(line_start_byte, line_end_after_newline_byte)`. Errors:
/// * `no_body_marker`: zero matching lines.
/// * `multiple_body_markers`: ≥ 2 matching lines.
///
/// Caveat: a line inside a `/-…-/` block comment whose trimmed content
/// is `-- BODY` will also match. Workers must not put the literal text
/// `-- BODY` (whitespace-trimmed, on its own line) inside any block
/// comment in a tablet file. Empirically 0/393 corpus files have such
/// a shape.
fn find_marker_line(content: &str) -> Result<(usize, usize), String> {
    let mut found: Option<(usize, usize)> = None;
    let mut second_at: Option<usize> = None;
    let mut offset = 0usize;
    for line in content.split_inclusive('\n') {
        let line_end = offset + line.len();
        let no_eol = line.trim_end_matches('\n').trim_end_matches('\r');
        if no_eol.trim() == BODY_MARKER_TRIMMED {
            if found.is_some() {
                second_at = Some(offset);
                break;
            }
            found = Some((offset, line_end));
        }
        offset = line_end;
    }
    if let Some(at) = second_at {
        let (first, _) = found.unwrap();
        return Err(format!(
            "filespec_split: multiple `-- BODY` marker lines (first at byte {first}, second at byte {at}); exactly one is required",
        ));
    }
    found.ok_or_else(|| {
        format!(
            "filespec_split: no `-- BODY` marker line found; expected exactly one line whose trimmed content is `{BODY_MARKER_TRIMMED}` between the principal declaration's signature and its proof body",
        )
    })
}

/// Locate the unique `-- [TABLET NODE: <Name>]` marker line. Returns
/// the first byte of that line. Two strict conditions on the file:
///   1. At least one line whose trimmed content starts with
///      `-- [TABLET NODE:` and ends with `]`.
///   2. Exactly one such line.
fn find_tablet_node_marker_line(content: &str) -> Result<usize, String> {
    let mut found: Option<usize> = None;
    let mut second_at: Option<usize> = None;
    let mut offset = 0usize;
    for line in content.split_inclusive('\n') {
        let no_eol = line.trim_end_matches('\n').trim_end_matches('\r');
        let trimmed = no_eol.trim();
        if trimmed.starts_with(TABLET_NODE_MARKER_PREFIX) && trimmed.ends_with(']') {
            if found.is_some() {
                second_at = Some(offset);
                break;
            }
            found = Some(offset);
        }
        offset += line.len();
    }
    if let Some(at) = second_at {
        let first = found.unwrap();
        return Err(format!(
            "filespec_split: multiple `-- [TABLET NODE: ...]` marker lines (first at byte {first}, second at byte {at}); exactly one is required",
        ));
    }
    found.ok_or_else(|| {
        "filespec_split: no `-- [TABLET NODE: <Name>]` marker line found; \
         every ordinary Tablet/<Node>.lean file must contain exactly one such line"
            .to_string()
    })
}

/// Locate every line whose trimmed content begins with `import `.
/// Returns the byte offset of the first byte of each such line. Used
/// to enforce the "imports must precede the tablet-node marker" rule
/// so that declaration_hash_strict's hashed slice cannot be poisoned
/// by smuggling an import into the protected region.
fn find_import_line_starts(content: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    for line in content.split_inclusive('\n') {
        let no_eol = line.trim_end_matches('\n').trim_end_matches('\r');
        if no_eol.trim_start().starts_with("import ") {
            out.push(offset);
        }
        offset += line.len();
    }
    out
}

/// Compute the FILESPEC-marker-based split for `content`. Pure-text,
/// no Lean / lake / filesystem access. The `_node` argument is
/// accepted for API symmetry with downstream callers and possible
/// future validation; this implementation ignores it because the
/// marker line is the boundary regardless of declaration name.
pub fn split(content: &str, node: &str) -> Result<FilespecSplit, String> {
    let (marker_start, marker_end) = find_marker_line(content)?;
    let source_hash = sha256_hex(content.as_bytes());
    Ok(FilespecSplit {
        node: NodeId::from(node),
        body_marker_start_byte: marker_start,
        body_marker_end_byte: marker_end,
        source_hash,
        record_version: RECORD_VERSION,
    })
}

/// Disk-reading convenience: reads `<repo>/Tablet/<node>.lean` and
/// splits it. Cache-free — the text scan is sub-millisecond.
pub fn fetch_split(repo_path: &Path, node: &str) -> Result<FilespecSplit, String> {
    let (content, _hash) = read_node_file(repo_path, node)?;
    split(&content, node)
}

/// FILESPEC validator. Surfaces a human-readable reason at worker
/// acceptance time. Four rules enforced here:
///   1. Exactly one `-- BODY` marker line.
///   2. Exactly one `-- [TABLET NODE: <Name>]` marker line.
///   3. The tablet-node marker line precedes the `-- BODY` marker,
///      and every `import` line precedes the tablet-node marker.
///   4. The line immediately preceding the `-- BODY` marker (after
///      trimming trailing whitespace) ends with `:=` or `by`. This
///      pins the marker to the FILESPEC-documented location — right
///      after the principal declaration's body delimiter — without
///      requiring the splitter to understand Lean syntax.
/// Rule (3) is what makes `declaration_hash_strict`'s
/// `[tablet_marker..body_marker]` slice stable under helper-import
/// additions: imports live in the unhashed preamble region by
/// FILESPEC fiat. The checker rejects any worker output that violates
/// any of these rules.
pub fn validate_filespec(content: &str, node: &str) -> Result<(), String> {
    let split_record = split(content, node)?;
    let tablet_marker_start = find_tablet_node_marker_line(content)?;
    if tablet_marker_start >= split_record.body_marker_start_byte {
        return Err(format!(
            "filespec_split: `-- [TABLET NODE: ...]` marker line (byte {tablet}) must precede `-- BODY` marker line (byte {body})",
            tablet = tablet_marker_start,
            body = split_record.body_marker_start_byte,
        ));
    }
    for import_start in find_import_line_starts(content) {
        if import_start >= tablet_marker_start {
            return Err(format!(
                "filespec_split: import line at byte {import_start} must precede the `-- [TABLET NODE: ...]` marker line at byte {tablet_marker_start}; \
                 imports live in the file preamble so the protected declaration-signature region (`[tablet_marker..body_marker]`) is stable under import additions",
            ));
        }
    }
    validate_body_marker_position(content, &split_record)?;
    Ok(())
}

/// Confirm the line immediately preceding the `-- BODY` marker (with
/// trailing whitespace ignored) ends with `:=` or `by`. The body
/// delimiter is either `:= by` (tactic mode) or `:=` followed by a
/// term that may sit below the marker (term mode), or a multi-line
/// `:= by` split where `by` is on its own line. All three end with
/// `:=` or `by` on the line immediately above the marker. Empty
/// content above the marker is rejected too — placing the marker at
/// column 0 of line 1 makes the file structurally broken.
fn validate_body_marker_position(
    content: &str,
    split_record: &FilespecSplit,
) -> Result<(), String> {
    // `pre_body` is the slice up to (not including) the first byte of
    // the `-- BODY` line, so it ends with the newline that terminates
    // the line above the marker. Strip exactly that one trailing
    // newline (and optional `\r` for CRLF) so a blank line above the
    // marker isn't silently skipped — blank-line-above is itself a
    // violation. Then the substring after the previous newline is
    // the actual prior line's content.
    let pre_body = &content[..split_record.body_marker_start_byte];
    let pre_body_no_eol = pre_body
        .strip_suffix("\r\n")
        .or_else(|| pre_body.strip_suffix('\n'))
        .unwrap_or(pre_body);
    let prior_line = pre_body_no_eol
        .rsplit_once('\n')
        .map(|(_, last)| last.trim_end_matches('\r'))
        .unwrap_or(pre_body_no_eol);
    let trimmed = prior_line.trim_end();
    if trimmed.is_empty() {
        return Err(
            "filespec_split: line immediately above `-- BODY` marker is empty; the marker must sit directly after the principal declaration's body delimiter (line ending with `:=` or `by`)".to_string(),
        );
    }
    let ends_with_assign = trimmed.ends_with(":=");
    // For "ends with `by`": accept either the bare word `by` on its
    // own (`prior_line` is exactly `by` after trimming whitespace) or
    // a preceding character that isn't an identifier-continuation,
    // so we don't accidentally accept tokens like `qualified.by` or
    // identifiers containing the substring `by`.
    let ends_with_by = if trimmed == "by" {
        true
    } else if let Some(stripped) = trimmed.strip_suffix("by") {
        match stripped.chars().last() {
            Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '.' => false,
            _ => true,
        }
    } else {
        false
    };
    if !(ends_with_assign || ends_with_by) {
        return Err(format!(
            "filespec_split: line immediately above `-- BODY` marker must end with `:=` or `by`; got {:?}",
            trimmed,
        ));
    }
    Ok(())
}

/// Normalise the statement region for hashing.
///
/// Identical post-processing to the legacy `normalize_declaration` in
/// `worker_normalization.rs`: trim, strip configured namespace
/// prefixes, collapse whitespace. The two sites must produce the same
/// output on the same logical input or capture-time and check-time
/// hashes diverge → false positives on proof-body edits.
fn normalize_declaration(decl: &str) -> String {
    let mut d = decl.trim().to_string();
    for prefix in NAMESPACE_PREFIXES {
        d = d.replace(prefix, "");
    }
    d.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Compute the production declaration hash for `node` over the
/// caller-supplied `lean_content`. The hashed region is
/// `content[tablet_marker_start_byte..body_marker_start_byte]` — the
/// span from (and including) the `-- [TABLET NODE: <Name>]` marker
/// line up to (but not including) the `-- BODY` marker line. This is
/// the declaration-signature region.
///
/// The file preamble — `import ...`, file-level `open ...`,
/// `set_option` at the file scope, leading comments — is **outside**
/// the hashed region by FILESPEC fiat: `validate_filespec` rejects
/// any `import` line that appears at or after the tablet-node marker,
/// so the hashed slice cannot be poisoned by smuggling imports into
/// the protected region. This lets workers add imports for new
/// helper nodes without tripping the kernel's signature-change gate
/// in `Local` mode (the symptom that motivated splitting the hash
/// region into preamble + protected on 2026-05-12).
pub fn declaration_hash_strict(
    _repo_path: &Path,
    lean_content: &str,
    node: &str,
) -> Result<String, String> {
    let split = split(lean_content, node)?;
    let tablet_marker_start = find_tablet_node_marker_line(lean_content)?;
    if tablet_marker_start >= split.body_marker_start_byte {
        return Err(format!(
            "filespec_split: `-- [TABLET NODE: ...]` marker line (byte {tablet}) must precede `-- BODY` marker line (byte {body})",
            tablet = tablet_marker_start,
            body = split.body_marker_start_byte,
        ));
    }
    let semantic = &lean_content[tablet_marker_start..split.body_marker_start_byte];
    Ok(sha256_hex(normalize_declaration(semantic).as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theorem_file_canonical() -> String {
        "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
theorem Foo : True := by
-- BODY
  trivial
"
        .to_string()
    }

    #[test]
    fn split_canonical_theorem() {
        let content = theorem_file_canonical();
        let s = split(&content, "Foo").expect("split must succeed");
        assert_eq!(s.node, NodeId::from("Foo"));
        let marker_line = &content[s.body_marker_start_byte..s.body_marker_end_byte];
        assert_eq!(marker_line.trim_end_matches('\n').trim(), "-- BODY");
        // Statement region includes everything up to (not including)
        // the marker line.
        let stmt = &content[..s.body_marker_start_byte];
        assert!(stmt.contains("theorem Foo : True"));
        assert!(stmt.contains(":= by"));
        // Body region is everything after the marker line.
        let body = &content[s.body_marker_end_byte..];
        assert!(body.contains("trivial"));
    }

    #[test]
    fn split_def_term_mode() {
        let content = "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
def Foo : Nat :=
-- BODY
  42
";
        let s = split(content, "Foo").expect("split must succeed");
        let stmt = &content[..s.body_marker_start_byte];
        assert!(stmt.contains("def Foo : Nat"));
        let body = &content[s.body_marker_end_byte..];
        assert!(body.contains("42"));
    }

    #[test]
    fn split_with_set_option_in_wrapper() {
        let content = "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
set_option maxHeartbeats 800000 in theorem Foo : True := by
-- BODY
  trivial
";
        let s = split(content, "Foo").expect("split must succeed");
        let stmt = &content[..s.body_marker_start_byte];
        assert!(stmt.contains("set_option maxHeartbeats 800000 in theorem Foo"));
    }

    #[test]
    fn split_with_multi_line_let_in_signature() {
        // The v1 plan's failure case. With the marker rule this is a
        // non-event — the splitter doesn't care about nested `:=`.
        let content = "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
theorem Foo :
    ∀ ε : ℝ, 0 < ε →
      let eps := min (ε / 2) (1 / 32 : ℝ)
      let eps1 := (1 / 2 : ℝ) * min eps (eps ^ 3 / 40)
      0 < eps ∧ eps < ε := by
-- BODY
  intro ε hε
  sorry
";
        let s = split(content, "Foo").expect("split must succeed");
        let stmt = &content[..s.body_marker_start_byte];
        assert!(stmt.contains("∀ ε"));
        assert!(stmt.contains("let eps :="));
        assert!(stmt.contains("0 < eps ∧ eps < ε"));
    }

    #[test]
    fn split_rejects_missing_marker() {
        let content = "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
theorem Foo : True := by
  trivial
";
        let err = split(content, "Foo").unwrap_err();
        assert!(err.contains("no `-- BODY` marker"));
    }

    #[test]
    fn split_rejects_multiple_markers() {
        let content = "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
theorem Foo : True := by
-- BODY
  trivial
-- BODY
";
        let err = split(content, "Foo").unwrap_err();
        assert!(err.contains("multiple `-- BODY` marker lines"));
    }

    #[test]
    fn split_marker_at_eof_without_trailing_newline() {
        // Last line has no trailing newline. Marker recognition must
        // still succeed.
        let content = "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
theorem Foo : True := by trivial
-- BODY";
        let s = split(content, "Foo").expect("split must succeed");
        assert_eq!(
            content[s.body_marker_start_byte..s.body_marker_end_byte].trim(),
            "-- BODY"
        );
    }

    #[test]
    fn validate_filespec_passes_on_canonical_file() {
        let content = theorem_file_canonical();
        assert!(validate_filespec(&content, "Foo").is_ok());
    }

    #[test]
    fn validate_filespec_fails_when_marker_absent() {
        let content = "theorem Foo : True := by\n  trivial\n";
        assert!(validate_filespec(content, "Foo").is_err());
    }

    #[test]
    fn declaration_hash_strict_is_stable_on_proof_body_edits() {
        // Same statement region, different proof body — hash must not
        // change. (The hashed region ends at the marker line, so any
        // change AFTER the marker line is invisible to the hash.)
        let v1 = "\
import Tablet.Preamble
-- [TABLET NODE: Foo]
theorem Foo : True := by
-- BODY
  trivial
";
        let v2 = "\
import Tablet.Preamble
-- [TABLET NODE: Foo]
theorem Foo : True := by
-- BODY
  exact True.intro
";
        let repo = std::path::Path::new("/dev/null");
        let h1 = declaration_hash_strict(repo, v1, "Foo").unwrap();
        let h2 = declaration_hash_strict(repo, v2, "Foo").unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn declaration_hash_strict_changes_on_signature_edit() {
        let v1 = "\
import Tablet.Preamble
-- [TABLET NODE: Foo]
theorem Foo : True := by
-- BODY
  trivial
";
        let v2 = "\
import Tablet.Preamble
-- [TABLET NODE: Foo]
theorem Foo : (1 : Nat) = 1 := by
-- BODY
  rfl
";
        let repo = std::path::Path::new("/dev/null");
        let h1 = declaration_hash_strict(repo, v1, "Foo").unwrap();
        let h2 = declaration_hash_strict(repo, v2, "Foo").unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn declaration_hash_strict_strips_namespace_prefixes() {
        let v1 = "\
import Tablet.Preamble
-- [TABLET NODE: Foo]
theorem Foo : Finset.range 3 = Finset.range 3 := by
-- BODY
  rfl
";
        let v2 = "\
import Tablet.Preamble
-- [TABLET NODE: Foo]
theorem Foo : range 3 = range 3 := by
-- BODY
  rfl
";
        let repo = std::path::Path::new("/dev/null");
        let h1 = declaration_hash_strict(repo, v1, "Foo").unwrap();
        let h2 = declaration_hash_strict(repo, v2, "Foo").unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn declaration_hash_strict_unaffected_by_added_imports() {
        // The 2026-05-12 (afternoon) regression: with the old hash
        // covering `[..body_marker]`, adding a helper import counted
        // as a signature change, forcing the kernel to demand
        // Restructure mode for routine import additions. The new
        // hash slice `[tablet_marker..body_marker]` excludes imports.
        let v1 = "\
import Tablet.Preamble
import Tablet.Helper1

-- [TABLET NODE: Foo]
theorem Foo : True := by
-- BODY
  trivial
";
        let v2 = "\
import Tablet.Preamble
import Tablet.Helper1
import Tablet.NewlyAddedHelper

-- [TABLET NODE: Foo]
theorem Foo : True := by
-- BODY
  trivial
";
        let repo = std::path::Path::new("/dev/null");
        let h1 = declaration_hash_strict(repo, v1, "Foo").unwrap();
        let h2 = declaration_hash_strict(repo, v2, "Foo").unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn declaration_hash_strict_unaffected_by_preamble_open_or_set_option() {
        // Same story for `open` and `set_option` lines in the file
        // preamble (above the tablet-node marker). The marker is the
        // boundary; everything above it can change freely.
        let v1 = "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
theorem Foo : True := by
-- BODY
  trivial
";
        let v2 = "\
import Tablet.Preamble

open Classical
set_option maxHeartbeats 800000

-- [TABLET NODE: Foo]
theorem Foo : True := by
-- BODY
  trivial
";
        let repo = std::path::Path::new("/dev/null");
        let h1 = declaration_hash_strict(repo, v1, "Foo").unwrap();
        let h2 = declaration_hash_strict(repo, v2, "Foo").unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn declaration_hash_strict_errors_when_tablet_marker_missing() {
        let content = "\
import Tablet.Preamble

theorem Foo : True := by
-- BODY
  trivial
";
        let repo = std::path::Path::new("/dev/null");
        let err = declaration_hash_strict(repo, content, "Foo").unwrap_err();
        assert!(err.contains("no `-- [TABLET NODE:"));
    }

    #[test]
    fn declaration_hash_strict_errors_when_tablet_marker_after_body() {
        // Malformed: tablet-node marker placed below the BODY marker.
        // No valid hash can be computed.
        let content = "\
import Tablet.Preamble
-- BODY
-- [TABLET NODE: Foo]
theorem Foo : True := by
  trivial
";
        let repo = std::path::Path::new("/dev/null");
        let err = declaration_hash_strict(repo, content, "Foo").unwrap_err();
        assert!(err.contains("must precede `-- BODY`"));
    }

    #[test]
    fn validate_filespec_rejects_imports_after_tablet_marker() {
        // Worker output that sneaks an import below the tablet-node
        // marker would poison the hash region. The checker rejects.
        let content = "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
import Tablet.SneakyHelper
theorem Foo : True := by
-- BODY
  trivial
";
        let err = validate_filespec(content, "Foo").unwrap_err();
        assert!(err.contains("import line"));
        assert!(err.contains("must precede"));
        assert!(err.contains("TABLET NODE"));
    }

    #[test]
    fn validate_filespec_passes_imports_all_above_tablet_marker() {
        let content = "\
import Tablet.Preamble
import Tablet.Helper1
import Tablet.Helper2

-- [TABLET NODE: Foo]
theorem Foo : True := by
-- BODY
  trivial
";
        assert!(validate_filespec(content, "Foo").is_ok());
    }

    #[test]
    fn validate_filespec_rejects_missing_tablet_marker() {
        let content = "\
import Tablet.Preamble

theorem Foo : True := by
-- BODY
  trivial
";
        let err = validate_filespec(content, "Foo").unwrap_err();
        assert!(err.contains("no `-- [TABLET NODE:"));
    }

    #[test]
    fn validate_filespec_rejects_multiple_tablet_markers() {
        let content = "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
-- [TABLET NODE: Foo]
theorem Foo : True := by
-- BODY
  trivial
";
        let err = validate_filespec(content, "Foo").unwrap_err();
        assert!(err.contains("multiple `-- [TABLET NODE"));
    }

    #[test]
    fn validate_filespec_accepts_tactic_mode_assign_by() {
        let content = "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
theorem Foo : True := by
-- BODY
  trivial
";
        assert!(validate_filespec(content, "Foo").is_ok());
    }

    #[test]
    fn validate_filespec_accepts_term_mode_bare_assign() {
        let content = "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
def Foo : Nat :=
-- BODY
  42
";
        assert!(validate_filespec(content, "Foo").is_ok());
    }

    #[test]
    fn validate_filespec_accepts_multi_line_by_split() {
        // `:=` and `by` on separate lines — the line immediately
        // above `-- BODY` ends with `by` alone. FILESPEC-allowed.
        let content = "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
theorem Foo : True :=
    by
-- BODY
  trivial
";
        assert!(validate_filespec(content, "Foo").is_ok());
    }

    #[test]
    fn validate_filespec_rejects_blank_line_above_body() {
        let content = "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
theorem Foo : True := by

-- BODY
  trivial
";
        let err = validate_filespec(content, "Foo").unwrap_err();
        assert!(err.contains("empty") || err.contains("end with `:=` or `by`"));
    }

    #[test]
    fn validate_filespec_rejects_body_in_middle_of_proof() {
        // Marker placed below a tactic line rather than above the
        // proof body. Worker mistake: simulates a worker that
        // inserted the marker in a random location.
        let content = "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
theorem Foo : True := by
  exact True.intro
-- BODY
  trivial
";
        let err = validate_filespec(content, "Foo").unwrap_err();
        assert!(err.contains("end with `:=` or `by`"));
    }

    #[test]
    fn validate_filespec_rejects_identifier_ending_in_by() {
        // `Bigby` ends with `by` substring but is an identifier, not
        // the Lean `by` keyword. Don't accept.
        let content = "\
import Tablet.Preamble

-- [TABLET NODE: Foo]
theorem Foo : True := Bigby
-- BODY
  trivial
";
        let err = validate_filespec(content, "Foo").unwrap_err();
        assert!(err.contains("end with `:=` or `by`"));
    }
}
