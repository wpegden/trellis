//! Stage 3 (PV-framework rebuild, plan doc 32, audit F8 / Codex 8): the
//! journal-naming sweep's acceptance check is a SCOPED banned-symbol test,
//! not a subjective repo-wide grep.  `kernel/src` must contain no occurrence
//! of the banned journal/PKI identifiers and messages outside a documented
//! allowlist of serde-legacy names (and the doc comments explaining their
//! legacy tolerance) that intentionally survive.

use std::fs;
use std::path::{Path, PathBuf};

/// The pinned banned list: identifiers and message fragments of the deleted
/// journal machinery (`journal.rs`), the ed25519 PKI (`auth.rs`), the
/// revision-closure store (`revision_store.rs`), and the detached Q7
/// package-authorization concept.  Deliberately NOT banned: the bare word
/// "journal" (the parallel-closure sidecar has its own unrelated apply
/// journal) and the tolerated serde-legacy keys listed in `ALLOWED`.
const BANNED: &[&str] = &[
    "TrustJournal",
    "ActorKeyManifest",
    "JournalEvent",
    "JournalEventPayload",
    "JournalPolicy",
    "JournalCheckpointBinding",
    "JournalRoutineGateOutcome",
    "JournalActor",
    "journal_predecessor_sha256",
    "revision_store",
    "publish_revision_closure",
    "load_revision_closure",
    "revision_closure_paths",
    "sign_actor_receipt",
    "sign_journal_commit_receipt",
    "verify_journal_commit_receipt",
    "verify_actor_receipt",
    "trust_actor_authentication_receipt",
    "actor_authentication_receipt",
    "external journal",
    "external-journal",
    "journal authorization",
    "sole-authority journal",
    "sole trust authority",
    "package-authorization-sidecar",
    "package_authorization_sidecar",
    "verify_authorized_package",
    "authorize_package",
];

/// Substrings whose enclosing LINE is tolerated: serde-legacy field names
/// that intentionally survive (`journal_checkpoint` /
/// `trust_journal_checkpoint` keys and their metadata siblings) need no
/// entry here because their names are not banned tokens; this list covers
/// the doc comments that explain what was deleted (they may name the
/// retired machinery while describing its retirement).
const ALLOWED_LINE_MARKERS: &[&str] = &[
    "plan doc 32",
    "Q1 (Stage 3",
    "Stage 3 (Q1",
    "died with",
    "retired",
    "deleted",
    "BANNED",
];

fn kernel_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read src dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn banned_journal_symbols_absent_outside_allowlist() {
    let mut sources = Vec::new();
    rust_sources(&kernel_src(), &mut sources);
    assert!(
        sources.len() > 10,
        "expected to scan the kernel source tree, found {} files",
        sources.len()
    );
    let mut violations = Vec::new();
    for path in sources {
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        // This test file itself pins the banned list; skip nothing else.
        for (line_number, line) in text.lines().enumerate() {
            let allowed = ALLOWED_LINE_MARKERS
                .iter()
                .any(|marker| line.contains(marker));
            if allowed {
                continue;
            }
            for banned in BANNED {
                if line.contains(banned) {
                    violations.push(format!(
                        "{}:{}: banned symbol {:?} in {:?}",
                        path.display(),
                        line_number + 1,
                        banned,
                        line.trim()
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "banned journal symbols survive outside the allowlist:\n{}",
        violations.join("\n")
    );
}

/// Removed trust-authority keys are not tolerated aliases. Old state must
/// fail closed rather than silently dropping or translating those values.
#[test]
fn removed_serde_authority_keys_are_not_tolerated() {
    let model = fs::read_to_string(kernel_src().join("model.rs")).expect("read model.rs");
    assert!(!model.contains("pub journal_checkpoint: Option<serde_json::Value>"));
    assert!(!model.contains("pub package_authorization_event_hash:"));
    assert!(model.contains("serde(default, deny_unknown_fields)"));
    let runtime = fs::read_to_string(kernel_src().join("runtime.rs")).expect("read runtime.rs");
    assert!(!runtime.contains("pub trust_journal_checkpoint:"));
}
