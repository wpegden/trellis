//! Minimal stub: the program-verification under-model support is not included
//! in this public release.
//!
//! The public interface below is retained so the ~kept call sites across the
//! kernel continue to compile. Non-PV runs never stage or approve assumptions,
//! so the write/approve/review entry points are inert no-ops here and the
//! read/detect entry points report "nothing pending".

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Claim-class tags carried on a proposed assumption record.
pub const CLAIM_CLASS_BEHAVIOR: &str = "behavior";
pub const CLAIM_CLASS_DOMAIN: &str = "domain";

/// Empty -> `behavior`.
pub fn normalize_claim_class(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        CLAIM_CLASS_BEHAVIOR.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Stub: statement-shape validation is not part of this public release.
pub fn staged_assumption_statement_errors(
    _lean_statement: &str,
    _claim_class: &str,
) -> Vec<String> {
    Vec::new()
}

/// The Preamble-tier Lean node every tablet node imports
/// (`import Tablet.Assumptions`).
pub const ASSUMPTIONS_NODE: &str = "Assumptions";
pub const RUST_VALIDITY_HOOK_NAME: &str = "RustValidSliceU8";
pub const RUST_VALIDITY_HOOK_TYPE: &str = "Slice Std.U8 → Prop";
pub const LEAN_ASSUMPTION_BEGIN_PREFIX: &str = "-- BEGIN UNDERMODEL ASSUMPTION ";
pub const LEAN_ASSUMPTION_END_PREFIX: &str = "-- END UNDERMODEL ASSUMPTION ";
pub const TEX_ASSUMPTION_BEGIN_PREFIX: &str = "% BEGIN UNDERMODEL ASSUMPTION ";
pub const TEX_ASSUMPTION_END_PREFIX: &str = "% END UNDERMODEL ASSUMPTION ";

fn assumptions_lean_path(repo_path: &Path) -> PathBuf {
    repo_path
        .join("Tablet")
        .join(format!("{ASSUMPTIONS_NODE}.lean"))
}

fn assumptions_tex_path(repo_path: &Path) -> PathBuf {
    repo_path
        .join("Tablet")
        .join(format!("{ASSUMPTIONS_NODE}.tex"))
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StagedAssumptionBlocks {
    pub lean_statement: String,
    pub nl_statement: String,
}

/// One assumption record. Retained as an inert data shell so callers that
/// construct or (de)serialize it continue to compile.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProposedAssumption {
    pub id: String,
    pub axiom_name: String,
    pub lean_statement: String,
    pub nl_statement: String,
    pub citation_locator: String,
    pub rust_justification: String,
    #[serde(default)]
    pub needed_by: Vec<String>,
    #[serde(default)]
    pub claim_class: String,
    pub adversarial_hunt_result: String,
    #[serde(default)]
    pub relativization_probe: String,
    pub status: String,
    #[serde(default)]
    pub rejected_reason: String,
}

/// The on-disk registry shape: a flat list of records.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProposedAssumptions {
    pub assumptions: Vec<ProposedAssumption>,
}

impl ProposedAssumptions {
    pub fn pending(&self) -> impl Iterator<Item = &ProposedAssumption> {
        self.assumptions.iter().filter(|a| a.status == "pending")
    }

    pub fn has_pending(&self) -> bool {
        self.pending().next().is_some()
    }
}

/// Stub: no provisionally-admitted axioms in this public release.
pub fn lane_passed_pending_axioms(_repo_path: &Path) -> Result<BTreeSet<String>, String> {
    Ok(BTreeSet::new())
}

/// Stub: no proposed-assumptions registry in this public release.
pub fn load_proposed(_repo_path: &Path) -> Result<ProposedAssumptions, String> {
    Ok(ProposedAssumptions::default())
}

/// Stub: never any pending assumptions.
pub fn has_pending(_repo_path: &Path) -> Result<bool, String> {
    Ok(false)
}

/// Stub: never any pending assumptions.
pub fn pending_count(_repo_path: &Path) -> Result<usize, String> {
    Ok(0)
}

/// Stub no-op.
pub fn record_proposed(_repo_path: &Path, _record: ProposedAssumption) -> Result<(), String> {
    Ok(())
}

/// Stub no-op.
pub fn reject(_repo_path: &Path, _id: &str, _reason: &str) -> Result<(), String> {
    Ok(())
}

/// Stub no-op.
pub fn project_all_pending(_repo_path: &Path) -> Result<usize, String> {
    Ok(0)
}

/// Stub no-op.
pub fn reject_all_pending(_repo_path: &Path, _reason: &str) -> Result<usize, String> {
    Ok(0)
}

/// Stub no-op.
pub fn project_approved(_repo_path: &Path, _id: &str) -> Result<(), String> {
    Ok(())
}

/// The seeded `Tablet/Assumptions.lean` scaffold: an inert Preamble-importing
/// shell holding the setup validity hook. Worker-staged blocks (when present)
/// sit between the marker comment lines.
pub fn empty_assumptions_lean_scaffold() -> String {
    format!(
        "import Tablet.Preamble\n\
         open Aeneas Aeneas.Std Result ControlFlow Error\n\n\
         axiom {RUST_VALIDITY_HOOK_NAME} : {RUST_VALIDITY_HOOK_TYPE}\n\n\
         -- Staged assumptions are written between the BEGIN/END marker comment lines.\n"
    )
}

/// The empty paired NL scaffold for `Tablet/Assumptions.tex`.
pub fn empty_assumptions_tex_scaffold() -> String {
    "% Staged assumptions are written between the BEGIN/END marker comment lines.\n".to_string()
}

fn validate_assumption_id(id: &str) -> Result<String, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("staged assumption id must be non-empty".to_string());
    }
    if id
        .chars()
        .any(|ch| !(ch == '_' || ch == '-' || ch == '.' || ch.is_ascii_alphanumeric()))
    {
        return Err(format!(
            "staged assumption id `{id}` may contain only ASCII letters, digits, `_`, `-`, and `.`"
        ));
    }
    Ok(id.to_string())
}

fn staged_begin_marker_id(line: &str, begin_prefix: &str) -> Option<String> {
    let id = line.trim().strip_prefix(begin_prefix)?.trim();
    validate_assumption_id(id).ok()
}

fn text_has_worker_authored_staged_begin_marker(text: &str, begin_prefix: &str) -> bool {
    text.lines()
        .any(|line| staged_begin_marker_id(line, begin_prefix).is_some())
}

/// True when `Tablet/Assumptions.{lean,tex}` contains a real worker-authored
/// staged-assumption begin marker (an exact marker line with a valid id).
pub fn has_worker_authored_staged_assumption(repo_path: &Path) -> Result<bool, String> {
    let lean_path = assumptions_lean_path(repo_path);
    let tex_path = assumptions_tex_path(repo_path);
    let lean_text = if lean_path.exists() {
        fs::read_to_string(&lean_path)
            .map_err(|err| format!("Failed to read {}: {err}", lean_path.display()))?
    } else {
        String::new()
    };
    let tex_text = if tex_path.exists() {
        fs::read_to_string(&tex_path)
            .map_err(|err| format!("Failed to read {}: {err}", tex_path.display()))?
    } else {
        String::new()
    };
    Ok(text_has_worker_authored_staged_begin_marker(
        &lean_text,
        LEAN_ASSUMPTION_BEGIN_PREFIX,
    ) || text_has_worker_authored_staged_begin_marker(
        &tex_text,
        TEX_ASSUMPTION_BEGIN_PREFIX,
    ))
}

/// Stub: staged-block extraction is not part of this public release.
pub fn extract_staged_assumption_blocks(
    _repo_path: &Path,
    _id: &str,
) -> Result<StagedAssumptionBlocks, String> {
    Ok(StagedAssumptionBlocks::default())
}

/// Stub no-op.
pub fn remove_staged_assumption_blocks(_repo_path: &Path, _id: &str) -> Result<(), String> {
    Ok(())
}

/// Stub no-op.
pub fn render_review(_repo_path: &Path) -> Result<(), String> {
    Ok(())
}
