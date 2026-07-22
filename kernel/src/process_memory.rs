//! Process memory — git-tracked, audit-adjudicated run knowledge
//! (`PROCESS_MEMORY_SPEC.md`).
//!
//! Storage is a tracked `process-memory/` directory in the tablet repo:
//! one entry file per claim (`<CoarseNode>/<entry-id>.md` or
//! `global/<entry-id>.md`) plus a kernel-regenerated `INDEX.md` of active
//! entries. Entry ids are kernel-assigned (`pm-<seq>-<kebab-title>`, monotone
//! `seq` from `ProtocolState.process_memory_seq`).
//!
//! Write authority is audit-only: `StuckMathAuditResponse.memory_operations`
//! carries `add` / `supersede` / `retire` operations; workers and reviewers
//! get a challenge channel (`memory_challenges`) recorded into
//! `ProtocolState.pending_memory_challenges` and adjudicated by the next
//! audit. The lifecycle is monotone — files are only added or status-flipped
//! forward (`active → superseded | retired`); no operation edits a body in
//! place, which makes the directory union-mergeable by file-level restore
//! (the LastClean carry-forward in `runtime.rs` exploits this).
//!
//! This module is the DISK side honored by the runtime for the engine's
//! `ApplyProcessMemoryOperations` command; the engine stays
//! deterministic-state-only (mirrors `assumptions_registry` / `dormant_store`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Repo-relative directory holding all process-memory entries.
pub const PROCESS_MEMORY_DIR: &str = "process-memory";

/// Size cap on an `add` / `supersede` body (spec §6 "under a size cap").
pub const PROCESS_MEMORY_BODY_MAX_CHARS: usize = 8000;

/// Cap on the pending-challenge queue so an agent loop cannot grow state
/// without bound; oldest entries win (later challenges are dropped until
/// an audit adjudicates).
pub const PROCESS_MEMORY_MAX_PENDING_CHALLENGES: usize = 32;

/// Allowed entry types (spec §3).
pub const PROCESS_MEMORY_ENTRY_TYPES: [&str; 5] = [
    "refuted-route",
    "constraint",
    "interface-decision",
    "counterexample",
    "process-note",
];

/// The coarse-node value naming the shared `global/` bucket.
pub const PROCESS_MEMORY_GLOBAL_CONE: &str = "global";

/// One raw memory operation as carried on the stuck-math-audit result
/// payload (spec §6). Field relevance is per-op: `add` uses
/// `type/coarse_node/title/body`; `supersede` uses
/// `entry_id/type/title/body`; `retire` uses `entry_id/reason`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryOperation {
    pub op: String,
    #[serde(default)]
    pub entry_id: String,
    #[serde(default, rename = "type")]
    pub entry_type: String,
    #[serde(default)]
    pub coarse_node: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub reason: String,
}

/// One worker/reviewer challenge against an active entry (spec §5).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryChallenge {
    pub entry_id: String,
    pub reason: String,
}

/// Parse + validate a raw `memory_challenges` list (worker or reviewer):
/// trimmed non-empty `entry_id` and `reason`, reason capped at
/// `AUDIT_TASK_REASON_MAX_CHARS`. Mirrors `parse_worker_audit_request` /
/// the reviewer `audit_request` normalization.
pub fn parse_memory_challenges(
    raw: &[MemoryChallenge],
) -> Result<Vec<MemoryChallenge>, String> {
    let mut out = Vec::with_capacity(raw.len());
    for (i, challenge) in raw.iter().enumerate() {
        let entry_id = challenge.entry_id.trim().to_string();
        let reason = challenge.reason.trim().to_string();
        if entry_id.is_empty() {
            return Err(format!(
                "memory_challenges[{i}].entry_id must name a process-memory entry"
            ));
        }
        if reason.is_empty() {
            return Err(format!(
                "memory_challenges[{i}].reason must state the contradicting evidence"
            ));
        }
        if reason.chars().count() > crate::model::AUDIT_TASK_REASON_MAX_CHARS {
            return Err(format!(
                "memory_challenges[{i}].reason must be at most {} characters",
                crate::model::AUDIT_TASK_REASON_MAX_CHARS
            ));
        }
        out.push(MemoryChallenge { entry_id, reason });
    }
    Ok(out)
}

/// A recorded challenge awaiting audit adjudication
/// (`ProtocolState.pending_memory_challenges`). `origin` is
/// `"worker"` / `"reviewer"`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PendingMemoryChallenge {
    pub origin: String,
    pub cycle: u32,
    pub request_id: u32,
    pub entry_id: String,
    pub reason: String,
}

/// One id-resolved, engine-authored file operation carried on
/// `ProtocolCommand::ApplyProcessMemoryOperations`. The engine assigns
/// ids (from `process_memory_seq`) and stamps provenance; the runtime
/// performs the writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ProcessMemoryFileOp {
    Add {
        entry_id: String,
        entry_type: String,
        coarse_node: String,
        title: String,
        body: String,
        cycle: u32,
        request_id: u32,
    },
    Supersede {
        /// The entry being superseded (must exist, status=active).
        entry_id: String,
        new_entry_id: String,
        entry_type: String,
        title: String,
        body: String,
        cycle: u32,
        request_id: u32,
    },
    Retire {
        entry_id: String,
        reason: String,
        cycle: u32,
        request_id: u32,
    },
}

/// Kebab-case an audit-authored title into the id slug component. Keeps
/// `[a-z0-9]` runs joined by `-`, capped so ids stay filename-friendly.
pub fn kebab_title(title: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in title.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(lower);
        } else {
            pending_dash = true;
        }
        if out.chars().count() >= 48 {
            break;
        }
    }
    if out.is_empty() {
        "entry".to_string()
    } else {
        out
    }
}

/// Kernel-assigned entry id: `pm-<seq>-<kebab-title>` (spec §2). `seq` is
/// zero-padded to four digits for stable lexicographic ordering of the
/// common case; wider values keep their natural width.
pub fn assign_entry_id(seq: u64, title: &str) -> String {
    format!("pm-{seq:04}-{}", kebab_title(title))
}

fn memory_root(repo_path: &Path) -> PathBuf {
    repo_path.join(PROCESS_MEMORY_DIR)
}

/// Minimal frontmatter view of one entry file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntryFrontmatter {
    pub id: String,
    pub entry_type: String,
    pub status: String,
    pub coarse_node: String,
}

/// Parse the `---`-fenced frontmatter of an entry file into
/// `(frontmatter, body)`. Line-based scalar parse only (the frontmatter is
/// kernel-authored; nested values are treated as opaque strings).
pub fn parse_entry(text: &str) -> Option<(EntryFrontmatter, String)> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut fm = EntryFrontmatter::default();
    let mut consumed = 1usize;
    let mut closed = false;
    for line in lines {
        consumed += 1;
        if line.trim() == "---" {
            closed = true;
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim().to_string();
            match key.trim() {
                "id" => fm.id = value,
                "type" => fm.entry_type = value,
                "status" => fm.status = value,
                "coarse_node" => fm.coarse_node = value,
                _ => {}
            }
        }
    }
    if !closed {
        return None;
    }
    let body = text
        .split_inclusive('\n')
        .skip(consumed)
        .collect::<String>()
        .trim()
        .to_string();
    Some((fm, body))
}

/// Locate an entry file by id anywhere under `process-memory/` (one level
/// of cone directories). Returns `None` when the directory or entry is
/// absent.
pub fn find_entry_file(repo_path: &Path, entry_id: &str) -> Option<PathBuf> {
    let root = memory_root(repo_path);
    let cones = fs::read_dir(&root).ok()?;
    for cone in cones.flatten() {
        let path = cone.path();
        if !path.is_dir() {
            continue;
        }
        let candidate = path.join(format!("{entry_id}.md"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Read the frontmatter of the entry with this id, when it exists.
pub fn read_entry_frontmatter(repo_path: &Path, entry_id: &str) -> Option<EntryFrontmatter> {
    let path = find_entry_file(repo_path, entry_id)?;
    let text = fs::read_to_string(path).ok()?;
    parse_entry(&text).map(|(fm, _)| fm)
}

/// Full entry ids whose `pm-<seq>` prefix matches a short-form `entry_id`
/// (e.g. `pm-0086` → `pm-0086-same-beta-...`). Used to make the
/// unknown-entry-id validation error actionable: audits reliably
/// abbreviate ids to the sequence prefix (stuck-math-audit 2998), and
/// the suggestion lets the retry converge on the exact id.
pub fn entry_ids_with_prefix(repo_path: &Path, entry_id: &str) -> Vec<String> {
    let prefix = format!("{entry_id}-");
    let mut matches = Vec::new();
    let Ok(cones) = fs::read_dir(memory_root(repo_path)) else {
        return matches;
    };
    for cone in cones.flatten() {
        let path = cone.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&path) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(stem) = name.strip_suffix(".md") {
                if stem.starts_with(&prefix) {
                    matches.push(stem.to_string());
                }
            }
        }
    }
    matches.sort();
    matches
}

/// Shape-only validation of raw `memory_operations` (no disk access):
/// legal op verbs, per-op required fields, body size cap, known entry
/// type, coarse_node ∈ coarse DAG ∪ "global", and no entry flipped twice
/// in one response. Shared by the artifact validator (which has no
/// request context — pass `None` to skip the coarse-node membership
/// check), the engine's deterministic validation, and the runtime CLI
/// checker (which layers the disk checks on top via
/// `validate_memory_operations_on_disk`).
pub fn validate_memory_operations_shape(
    ops: &[MemoryOperation],
    coarse_dag_nodes: Option<&BTreeSet<crate::model::NodeId>>,
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut flipped: BTreeSet<&str> = BTreeSet::new();
    for (i, op) in ops.iter().enumerate() {
        let verb = op.op.trim();
        match verb {
            "add" | "supersede" => {
                if !PROCESS_MEMORY_ENTRY_TYPES.contains(&op.entry_type.trim()) {
                    errors.push(format!(
                        "memory_operations[{i}].type must be one of {PROCESS_MEMORY_ENTRY_TYPES:?}"
                    ));
                }
                if op.title.trim().is_empty() {
                    errors.push(format!("memory_operations[{i}].title must be non-empty"));
                }
                if op.body.trim().is_empty() {
                    errors.push(format!("memory_operations[{i}].body must be non-empty"));
                }
                if op.body.chars().count() > PROCESS_MEMORY_BODY_MAX_CHARS {
                    errors.push(format!(
                        "memory_operations[{i}].body must be at most {PROCESS_MEMORY_BODY_MAX_CHARS} characters"
                    ));
                }
                if verb == "add" {
                    let cone = op.coarse_node.trim();
                    if cone.is_empty() {
                        errors.push(format!(
                            "memory_operations[{i}].coarse_node must be a coarse node or \"{PROCESS_MEMORY_GLOBAL_CONE}\""
                        ));
                    } else if let Some(coarse) = coarse_dag_nodes {
                        let known = cone == PROCESS_MEMORY_GLOBAL_CONE
                            || coarse.contains(&crate::model::NodeId::from(cone));
                        if !known {
                            errors.push(format!(
                                "memory_operations[{i}].coarse_node `{cone}` must be a known coarse node or \"{PROCESS_MEMORY_GLOBAL_CONE}\""
                            ));
                        }
                    }
                }
            }
            "retire" => {
                if op.reason.trim().is_empty() {
                    errors.push(format!(
                        "memory_operations[{i}].reason must be non-empty for retire"
                    ));
                }
            }
            _ => {
                errors.push(format!(
                    "memory_operations[{i}].op must be one of ['add', 'supersede', 'retire']"
                ));
                continue;
            }
        }
        if verb == "supersede" || verb == "retire" {
            let entry_id = op.entry_id.trim();
            if entry_id.is_empty() {
                errors.push(format!(
                    "memory_operations[{i}].entry_id must be non-empty for {verb}"
                ));
            } else if !flipped.insert(entry_id) {
                errors.push(format!(
                    "memory_operations[{i}].entry_id `{entry_id}` is flipped more than once in this response"
                ));
            }
        }
    }
    errors
}

/// Disk-dependent validation (spec §6): every `supersede`/`retire`
/// `entry_id` must name an existing entry with `status: active`. Run by
/// the runtime CLI checker, which has the repo worktree; the engine's
/// pure validation covers the shape half.
pub fn validate_memory_operations_on_disk(
    ops: &[MemoryOperation],
    repo_path: &Path,
) -> Vec<String> {
    let mut errors = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let verb = op.op.trim();
        if verb != "supersede" && verb != "retire" {
            continue;
        }
        let entry_id = op.entry_id.trim();
        if entry_id.is_empty() {
            continue; // shape validation already flagged it
        }
        match read_entry_frontmatter(repo_path, entry_id) {
            None => {
                let prefix_matches = entry_ids_with_prefix(repo_path, entry_id);
                let hint = if prefix_matches.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; entry ids are the full `pm-<seq>-<slug>` form — did you mean {}?",
                        prefix_matches
                            .iter()
                            .map(|id| format!("`{id}`"))
                            .collect::<Vec<_>>()
                            .join(" or ")
                    )
                };
                errors.push(format!(
                    "memory_operations[{i}].entry_id `{entry_id}` does not name an existing process-memory entry{hint}"
                ));
            }
            Some(fm) if fm.status != "active" => errors.push(format!(
                "memory_operations[{i}].entry_id `{entry_id}` has status `{}`; only active entries may be {verb}d",
                fm.status
            )),
            Some(_) => {}
        }
    }
    errors
}

fn quote_frontmatter_string(raw: &str) -> String {
    let flat: String = raw
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    format!("\"{}\"", flat.replace('\\', "\\\\").replace('"', "\\\""))
}

fn render_entry_file(
    entry_id: &str,
    entry_type: &str,
    coarse_node: &str,
    body: &str,
    cycle: u32,
    request_id: u32,
) -> String {
    format!(
        "---\nid: {entry_id}\ntype: {entry_type}\nstatus: active\ncoarse_node: {coarse_node}\ncreated: {{cycle: {cycle}, request_id: {request_id}}}\n---\n\n{}\n",
        body.trim()
    )
}

/// Flip one frontmatter status forward, inserting any extra frontmatter
/// lines directly after the status line. Body bytes are untouched
/// (monotonicity invariant, spec §4).
fn flip_entry_status(
    path: &Path,
    new_status: &str,
    extra_lines: &[String],
) -> Result<(), String> {
    let text = fs::read_to_string(path)
        .map_err(|err| format!("failed to read process-memory entry {}: {err}", path.display()))?;
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    let mut in_frontmatter = false;
    for (idx, line) in text.lines().enumerate() {
        if idx == 0 && line.trim() == "---" {
            in_frontmatter = true;
            out.push(line.to_string());
            continue;
        }
        if in_frontmatter && line.trim() == "---" {
            in_frontmatter = false;
            out.push(line.to_string());
            continue;
        }
        if in_frontmatter && !replaced && line.trim_start().starts_with("status:") {
            out.push(format!("status: {new_status}"));
            out.extend(extra_lines.iter().cloned());
            replaced = true;
            continue;
        }
        out.push(line.to_string());
    }
    if !replaced {
        return Err(format!(
            "process-memory entry {} has no status frontmatter line",
            path.display()
        ));
    }
    fs::write(path, out.join("\n") + "\n")
        .map_err(|err| format!("failed to write process-memory entry {}: {err}", path.display()))
}

/// Apply the engine-authored file operations and regenerate `INDEX.md`.
/// Fails loudly (the caller aborts the runtime step) on any
/// inconsistency — the operations were validated against this same
/// worktree at artifact-check time, so a failure here is real skew.
pub fn apply_file_ops(repo_path: &Path, ops: &[ProcessMemoryFileOp]) -> Result<(), String> {
    for op in ops {
        match op {
            ProcessMemoryFileOp::Add {
                entry_id,
                entry_type,
                coarse_node,
                title: _,
                body,
                cycle,
                request_id,
            } => {
                write_new_entry(
                    repo_path, coarse_node, entry_id, entry_type, body, *cycle, *request_id,
                )?;
            }
            ProcessMemoryFileOp::Supersede {
                entry_id,
                new_entry_id,
                entry_type,
                title: _,
                body,
                cycle,
                request_id,
            } => {
                let old_path = find_entry_file(repo_path, entry_id).ok_or_else(|| {
                    format!("supersede target `{entry_id}` not found under process-memory/")
                })?;
                let cone_dir = old_path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| {
                        format!("cannot resolve cone directory for `{entry_id}`")
                    })?
                    .to_string();
                flip_entry_status(
                    &old_path,
                    "superseded",
                    &[format!("superseded_by: {new_entry_id}")],
                )?;
                write_new_entry(
                    repo_path,
                    &cone_dir,
                    new_entry_id,
                    entry_type,
                    body,
                    *cycle,
                    *request_id,
                )?;
            }
            ProcessMemoryFileOp::Retire {
                entry_id,
                reason,
                cycle,
                request_id,
            } => {
                let path = find_entry_file(repo_path, entry_id).ok_or_else(|| {
                    format!("retire target `{entry_id}` not found under process-memory/")
                })?;
                flip_entry_status(
                    &path,
                    "retired",
                    &[format!(
                        "retired: {{cycle: {cycle}, request_id: {request_id}, reason: {}}}",
                        quote_frontmatter_string(reason.trim())
                    )],
                )?;
            }
        }
    }
    regenerate_index(repo_path)
}

fn write_new_entry(
    repo_path: &Path,
    coarse_node: &str,
    entry_id: &str,
    entry_type: &str,
    body: &str,
    cycle: u32,
    request_id: u32,
) -> Result<(), String> {
    let cone = coarse_node.trim();
    let cone = if cone.is_empty() {
        PROCESS_MEMORY_GLOBAL_CONE
    } else {
        cone
    };
    let dir = memory_root(repo_path).join(cone);
    fs::create_dir_all(&dir)
        .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    let path = dir.join(format!("{entry_id}.md"));
    if path.exists() {
        return Err(format!(
            "process-memory entry {} already exists; ids must be unique (check process_memory_seq)",
            path.display()
        ));
    }
    fs::write(
        &path,
        render_entry_file(entry_id, entry_type, cone, body, cycle, request_id),
    )
    .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

/// One `(frontmatter, hook)` row for the index: hook = first non-empty
/// body line, truncated.
fn index_rows(repo_path: &Path) -> Result<Vec<(EntryFrontmatter, String)>, String> {
    let root = memory_root(repo_path);
    let mut rows = Vec::new();
    let cones = match fs::read_dir(&root) {
        Ok(cones) => cones,
        Err(_) => return Ok(rows),
    };
    for cone in cones.flatten() {
        let cone_path = cone.path();
        if !cone_path.is_dir() {
            continue;
        }
        let entries = fs::read_dir(&cone_path)
            .map_err(|err| format!("failed to list {}: {err}", cone_path.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let text = fs::read_to_string(&path)
                .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
            let Some((fm, body)) = parse_entry(&text) else {
                continue;
            };
            let hook: String = body
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("")
                .trim()
                .chars()
                .take(160)
                .collect();
            rows.push((fm, hook));
        }
    }
    rows.sort_by(|a, b| a.0.id.cmp(&b.0.id));
    Ok(rows)
}

/// Regenerate `process-memory/INDEX.md` from the entry files: one line
/// per ACTIVE entry (spec §3). Derived state — the kernel is the only
/// writer.
pub fn regenerate_index(repo_path: &Path) -> Result<(), String> {
    let root = memory_root(repo_path);
    if !root.is_dir() {
        return Ok(());
    }
    let mut lines = vec![
        "# Process memory index".to_string(),
        String::new(),
        "Kernel-generated; one line per ACTIVE entry. Full entries live in the".to_string(),
        "per-cone files. Do not hand-edit.".to_string(),
        String::new(),
    ];
    for (fm, hook) in index_rows(repo_path)? {
        if fm.status != "active" {
            continue;
        }
        lines.push(format!(
            "- [{}] {}/{} — {}",
            fm.id, fm.entry_type, fm.coarse_node, hook
        ));
    }
    let path = root.join("INDEX.md");
    fs::write(&path, lines.join("\n") + "\n")
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo() -> tempfile::TempDir {
        // Local .tmp-tests root (never /tmp): mirrors runtime.rs's
        // `local_tempdir` convention.
        let tmp_root = std::env::current_dir()
            .expect("current dir")
            .join(".tmp-tests");
        std::fs::create_dir_all(&tmp_root).expect("tmp root");
        tempfile::tempdir_in(&tmp_root).expect("tempdir")
    }

    fn add_op(entry_id: &str, coarse: &str) -> ProcessMemoryFileOp {
        ProcessMemoryFileOp::Add {
            entry_id: entry_id.to_string(),
            entry_type: "refuted-route".to_string(),
            coarse_node: coarse.to_string(),
            title: "t".to_string(),
            body: "Route X is refuted: counterexample n=3.".to_string(),
            cycle: 7,
            request_id: 41,
        }
    }

    #[test]
    fn kebab_and_id_assignment() {
        assert_eq!(
            kebab_title("Invariant Regeneration -- REFUTED!"),
            "invariant-regeneration-refuted"
        );
        assert_eq!(kebab_title("   "), "entry");
        assert_eq!(
            assign_entry_id(42, "Invariant regeneration refuted"),
            "pm-0042-invariant-regeneration-refuted"
        );
        assert_eq!(assign_entry_id(12345, "x"), "pm-12345-x");
    }

    #[test]
    fn shape_validation_rejects_each_reason() {
        let coarse: BTreeSet<crate::model::NodeId> =
            [crate::model::NodeId::from("SomeCoarseNode")].into();
        let bad_verb = MemoryOperation {
            op: "delete".into(),
            ..MemoryOperation::default()
        };
        assert!(validate_memory_operations_shape(&[bad_verb], Some(&coarse))[0].contains("op must be"));

        let bad_type = MemoryOperation {
            op: "add".into(),
            entry_type: "hunch".into(),
            coarse_node: "global".into(),
            title: "t".into(),
            body: "b".into(),
            ..MemoryOperation::default()
        };
        assert!(validate_memory_operations_shape(&[bad_type], Some(&coarse))
            .iter()
            .any(|e| e.contains("type must be")));

        let empty_body = MemoryOperation {
            op: "add".into(),
            entry_type: "constraint".into(),
            coarse_node: "global".into(),
            title: "t".into(),
            body: "  ".into(),
            ..MemoryOperation::default()
        };
        assert!(validate_memory_operations_shape(&[empty_body], Some(&coarse))
            .iter()
            .any(|e| e.contains("body must be non-empty")));

        let big_body = MemoryOperation {
            op: "add".into(),
            entry_type: "constraint".into(),
            coarse_node: "global".into(),
            title: "t".into(),
            body: "x".repeat(PROCESS_MEMORY_BODY_MAX_CHARS + 1),
            ..MemoryOperation::default()
        };
        assert!(validate_memory_operations_shape(&[big_body], Some(&coarse))
            .iter()
            .any(|e| e.contains("at most")));

        let bad_cone = MemoryOperation {
            op: "add".into(),
            entry_type: "constraint".into(),
            coarse_node: "NoSuchNode".into(),
            title: "t".into(),
            body: "b".into(),
            ..MemoryOperation::default()
        };
        assert!(validate_memory_operations_shape(&[bad_cone], Some(&coarse))
            .iter()
            .any(|e| e.contains("known coarse node")));

        let known_cone = MemoryOperation {
            op: "add".into(),
            entry_type: "constraint".into(),
            coarse_node: "SomeCoarseNode".into(),
            title: "t".into(),
            body: "b".into(),
            ..MemoryOperation::default()
        };
        assert!(validate_memory_operations_shape(&[known_cone], Some(&coarse)).is_empty());

        let retire_no_reason = MemoryOperation {
            op: "retire".into(),
            entry_id: "pm-0001-x".into(),
            ..MemoryOperation::default()
        };
        assert!(validate_memory_operations_shape(&[retire_no_reason], Some(&coarse))
            .iter()
            .any(|e| e.contains("reason must be non-empty")));

        let retire_no_id = MemoryOperation {
            op: "retire".into(),
            reason: "faulty".into(),
            ..MemoryOperation::default()
        };
        assert!(validate_memory_operations_shape(&[retire_no_id], Some(&coarse))
            .iter()
            .any(|e| e.contains("entry_id must be non-empty")));

        let double_flip = [
            MemoryOperation {
                op: "retire".into(),
                entry_id: "pm-0001-x".into(),
                reason: "faulty".into(),
                ..MemoryOperation::default()
            },
            MemoryOperation {
                op: "supersede".into(),
                entry_id: "pm-0001-x".into(),
                entry_type: "constraint".into(),
                title: "t".into(),
                body: "b".into(),
                ..MemoryOperation::default()
            },
        ];
        assert!(validate_memory_operations_shape(&double_flip, Some(&coarse))
            .iter()
            .any(|e| e.contains("flipped more than once")));
    }

    #[test]
    fn disk_validation_short_form_entry_id_gets_full_id_suggestion() {
        // Regression (stuck-math-audit 2998, cycle 677): the audit
        // superseded `pm-0086` / `pm-0087` using the `pm-<seq>` short form
        // while the entries on disk carry full `pm-<seq>-<slug>` ids. The
        // rejection is correct, but the error must be actionable — name the
        // offending op AND suggest the full id so the retry converges.
        // Note the temp repo is not even a git repo: disk validation reads
        // the worktree directly, so post-rewind present-but-untracked
        // entries validate identically to committed ones.
        let dir = temp_repo();
        let repo = dir.path();
        apply_file_ops(repo, &[add_op("pm-0001-route-x", "ConeA")]).unwrap();

        let short = MemoryOperation {
            op: "supersede".into(),
            entry_id: "pm-0001".into(),
            entry_type: "refuted-route".into(),
            title: "t".into(),
            body: "b".into(),
            ..MemoryOperation::default()
        };
        let errors = validate_memory_operations_on_disk(&[short], repo);
        assert_eq!(errors.len(), 1, "errors={errors:?}");
        assert!(
            errors[0].contains("memory_operations[0].entry_id `pm-0001`"),
            "error must name the offending op: {}",
            errors[0]
        );
        assert!(
            errors[0].contains("did you mean `pm-0001-route-x`"),
            "error must suggest the full entry id: {}",
            errors[0]
        );

        // The full id validates cleanly against the same (untracked) state.
        let full = MemoryOperation {
            op: "supersede".into(),
            entry_id: "pm-0001-route-x".into(),
            entry_type: "refuted-route".into(),
            title: "t".into(),
            body: "b".into(),
            ..MemoryOperation::default()
        };
        assert!(validate_memory_operations_on_disk(&[full], repo).is_empty());

        // A short id with no matching entry keeps the plain error.
        let unknown = MemoryOperation {
            op: "retire".into(),
            entry_id: "pm-9999".into(),
            reason: "r".into(),
            ..MemoryOperation::default()
        };
        let errors = validate_memory_operations_on_disk(&[unknown], repo);
        assert_eq!(errors.len(), 1);
        assert!(!errors[0].contains("did you mean"), "{}", errors[0]);
    }

    #[test]
    fn apply_add_supersede_retire_and_index() {
        let dir = temp_repo();
        let repo = dir.path();

        apply_file_ops(repo, &[add_op("pm-0001-route-x", "ConeA")]).unwrap();
        let path = repo.join("process-memory/ConeA/pm-0001-route-x.md");
        assert!(path.is_file());
        let (fm, body) = parse_entry(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(fm.status, "active");
        assert_eq!(fm.coarse_node, "ConeA");
        assert!(body.contains("counterexample n=3"));
        let index = fs::read_to_string(repo.join("process-memory/INDEX.md")).unwrap();
        assert!(index.contains("- [pm-0001-route-x] refuted-route/ConeA"));

        // Disk validation: active entry passes, unknown entry fails.
        let ok = validate_memory_operations_on_disk(
            &[MemoryOperation {
                op: "retire".into(),
                entry_id: "pm-0001-route-x".into(),
                reason: "r".into(),
                ..MemoryOperation::default()
            }],
            repo,
        );
        assert!(ok.is_empty());
        let missing = validate_memory_operations_on_disk(
            &[MemoryOperation {
                op: "retire".into(),
                entry_id: "pm-9999-nope".into(),
                reason: "r".into(),
                ..MemoryOperation::default()
            }],
            repo,
        );
        assert!(missing[0].contains("does not name an existing"));

        // Supersede keeps the tombstone and writes the new entry in the
        // same cone dir.
        apply_file_ops(
            repo,
            &[ProcessMemoryFileOp::Supersede {
                entry_id: "pm-0001-route-x".into(),
                new_entry_id: "pm-0002-route-x-narrowed".into(),
                entry_type: "refuted-route".into(),
                title: "t".into(),
                body: "Narrowed: route X fails only for n>2.".into(),
                cycle: 9,
                request_id: 55,
            }],
        )
        .unwrap();
        let old = fs::read_to_string(&path).unwrap();
        assert!(old.contains("status: superseded"));
        assert!(old.contains("superseded_by: pm-0002-route-x-narrowed"));
        assert!(old.contains("counterexample n=3"), "body must be untouched");
        let index = fs::read_to_string(repo.join("process-memory/INDEX.md")).unwrap();
        assert!(!index.contains("pm-0001-route-x]"));
        assert!(index.contains("- [pm-0002-route-x-narrowed]"));

        // Superseded entries are no longer valid supersede/retire targets.
        let stale = validate_memory_operations_on_disk(
            &[MemoryOperation {
                op: "retire".into(),
                entry_id: "pm-0001-route-x".into(),
                reason: "r".into(),
                ..MemoryOperation::default()
            }],
            repo,
        );
        assert!(stale[0].contains("only active entries"));

        // Retire flips status + records the reason; index drops the entry.
        apply_file_ops(
            repo,
            &[ProcessMemoryFileOp::Retire {
                entry_id: "pm-0002-route-x-narrowed".into(),
                reason: "counterexample was mis-scaled; see pm-0003".into(),
                cycle: 12,
                request_id: 77,
            }],
        )
        .unwrap();
        let retired = fs::read_to_string(
            repo.join("process-memory/ConeA/pm-0002-route-x-narrowed.md"),
        )
        .unwrap();
        assert!(retired.contains("status: retired"));
        assert!(retired.contains("reason: \"counterexample was mis-scaled"));
        let index = fs::read_to_string(repo.join("process-memory/INDEX.md")).unwrap();
        assert!(!index.contains("pm-0002-route-x-narrowed]"));

        // Duplicate add id fails loudly.
        let dup = apply_file_ops(repo, &[add_op("pm-0002-route-x-narrowed", "ConeA")]);
        assert!(dup.is_err());
    }
}
