//! Parallel-closure sidecar — ingest-surface tests (SIDECAR plan
//! commit 5; test plan §6.1).
//!
//! Tiers covered here:
//!   * the K§2.2 drift-disposition table, one named case per row, over
//!     the pure `preflight_claimed_attempt` decision fn;
//!   * write-ahead journal recovery — five cases incl. amendment A4
//!     (orphaned `claimed/` sweep) and amendment A7 (the
//!     commit-before-event-persist crash window);
//!   * spool claim/defer/finalize mechanics (rename-claim, D2
//!     defer-then-reject ceiling, verdict append preserving unknown
//!     daemon fields);
//!   * only-BODY splice adversarial property (bodies containing
//!     `-- BODY`, whole-file emissions, unicode) — the prefix is
//!     byte-frozen and smuggled markers die at `validate_filespec`;
//!   * the A3 sidecar ban scan (kernel-side body gate);
//!   * two-repo worker-mirror layout detection.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use trellis_kernel::sidecar::{
    claim_attempt, defer_attempt, ensure_spool_dirs, finalize_attempt, next_pending_attempt,
    parse_attempt_record, preflight_claimed_attempt, run_journal_recovery, sidecar_body_ban_scan,
    spool_dirs, splice_proof_body, supervisor_workspace_tablet_dir, sweep_orphaned_claims,
    write_apply_journal, DeferOutcome, JournalRecoveryOutcome, SidecarApplyJournal,
    SidecarPreflight, SidecarRuntimeConfig,
};
use trellis_kernel::filespec_split::validate_filespec;
use trellis_kernel::{CorrStatus, NodeId, NodeKind, Phase, ProtocolState, TargetId};

fn tempdir() -> tempfile::TempDir {
    let tmp_root = std::env::current_dir()
        .expect("current dir")
        .join(".tmp-tests");
    std::fs::create_dir_all(&tmp_root).expect("tmp root");
    tempfile::tempdir_in(&tmp_root).expect("tempdir")
}

fn node(id: &str) -> NodeId {
    NodeId::from(id)
}

const NODE_FILE: &str = "import Tablet.Preamble\n\n-- [TABLET NODE: Rung]\ntheorem Rung : True := by\n-- BODY\n  sorry\n";

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn eligible_state() -> ProtocolState {
    let mut state = ProtocolState::default();
    state.phase = Phase::ProofFormalization;
    let n = node("Rung");
    state.live.present_nodes.insert(n.clone());
    state.live.open_nodes.insert(n.clone());
    state.node_kinds.insert(n.clone(), NodeKind::Proof);
    state.proof_nodes.insert(n.clone());
    state.corr_status.insert(n.clone(), CorrStatus::Pass);
    state
        .live
        .corr_current_fingerprints
        .insert(n.clone(), "c1".to_string());
    state
        .corr_approved_fingerprints
        .insert(n.clone(), "c1".to_string());
    state.substantiveness_status.insert(n.clone(), CorrStatus::Pass);
    state
        .live
        .substantiveness_current_fingerprints
        .insert(n.clone(), "s1".to_string());
    state
        .substantiveness_approved_fingerprints
        .insert(n.clone(), "s1".to_string());
    // Queue redesign: the apply path requires the node QUEUED with a
    // matching generation; the baseline state queues Rung at seq 7 (the
    // record fixture below carries the same).
    state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: n.clone(),
        entry_seq: 7,
        queued_at_cycle: 0,
    });
    state.sidecar_queue_seq = 7;
    state
}

fn attempt_record_json(node_file_sha: &str, body: &str) -> String {
    serde_json::json!({
        "schema": 2,
        "attempt_id": "sc-20260722-213301-Rung",
        "node": "Rung",
        "snapshot_sha": "deadbeef",
        "entry_seq": 7,
        "base": {
            "node_file_sha256": node_file_sha,
            "statement_prefix_sha256": "sp"
        },
        "artifact": { "proof_body": body },
        "status": "success",
        "provenance": {
            "provider": "mistral", "model": "labs-leanstral-1-5",
            "iterations": 7, "wall_secs": 811.4,
            "tokens": {"prompt": 231044, "completion": 48211},
            "driver_version": "test"
        },
        "daemon_validation": { "compiled": true }
    })
    .to_string()
}

fn record_for_content(content: &str, body: &str) -> trellis_kernel::sidecar::SidecarAttemptRecord {
    parse_attempt_record(&attempt_record_json(&sha256_hex(content.as_bytes()), body))
        .expect("parse attempt")
}

fn cfg() -> SidecarRuntimeConfig {
    SidecarRuntimeConfig::default()
}

// ====================================================================
// K§2.2 drift-disposition table — one named test per row
// ====================================================================

#[test]
fn drift_row_unchanged_content_proceeds() {
    let state = eligible_state();
    let record = record_for_content(NODE_FILE, "  trivial\n");
    assert_eq!(
        preflight_claimed_attempt(&state, &cfg(), &record, Some(NODE_FILE)),
        SidecarPreflight::Proceed
    );
}

#[test]
fn drift_row_worker_statement_edit_rejects_stale_content() {
    let state = eligible_state();
    let record = record_for_content(NODE_FILE, "  trivial\n");
    let edited = NODE_FILE.replace("True", "1 = 1");
    let decision = preflight_claimed_attempt(&state, &cfg(), &record, Some(&edited));
    assert_eq!(
        decision,
        SidecarPreflight::Reject {
            reason: "stale_content".to_string()
        }
    );
}

#[test]
fn drift_row_worker_body_edit_rejects_stale_content() {
    let state = eligible_state();
    let record = record_for_content(NODE_FILE, "  trivial\n");
    let edited = NODE_FILE.replace("  sorry", "  have h : True := trivial\n  sorry");
    let decision = preflight_claimed_attempt(&state, &cfg(), &record, Some(&edited));
    assert_eq!(
        decision,
        SidecarPreflight::Reject {
            reason: "stale_content".to_string()
        }
    );
}

#[test]
fn drift_row_dep_statement_edit_proceeds_to_revalidation() {
    // A DEP's statement moved but this node's file is byte-identical:
    // content-addressed gate passes; the compile/probe revalidation
    // against the CURRENT tree decides. (The dep pre-filter is a
    // daemon-side skip-cheap signal, not a kernel gate.)
    let mut state = eligible_state();
    state
        .live
        .corr_current_fingerprints
        .insert(node("Dep"), "moved".to_string());
    let record = record_for_content(NODE_FILE, "  trivial\n");
    assert_eq!(
        preflight_claimed_attempt(&state, &cfg(), &record, Some(NODE_FILE)),
        SidecarPreflight::Proceed
    );
}

#[test]
fn drift_row_closed_by_primary_rejects_ineligible() {
    let mut state = eligible_state();
    state.live.open_nodes.remove(&node("Rung"));
    let record = record_for_content(NODE_FILE, "  trivial\n");
    let decision = preflight_claimed_attempt(&state, &cfg(), &record, Some(NODE_FILE));
    assert_eq!(
        decision,
        SidecarPreflight::Reject {
            reason: "ineligible".to_string()
        }
    );
}

#[test]
fn drift_row_deleted_or_renamed_rejects_ineligible() {
    let mut state = eligible_state();
    state.live.present_nodes.remove(&node("Rung"));
    state.live.open_nodes.remove(&node("Rung"));
    let record = record_for_content(NODE_FILE, "  trivial\n");
    assert_eq!(
        preflight_claimed_attempt(&state, &cfg(), &record, None),
        SidecarPreflight::Reject {
            reason: "ineligible".to_string()
        }
    );
}

#[test]
fn drift_row_lastclean_rewind_content_match_survives() {
    // After a LastClean rewind, HEAD moved but the rewound tree's node
    // file equals the pre-image ⇒ content check passes — the attempt
    // survives with NO special-casing (content-addressed beats
    // commit-addressed; the record's snapshot_sha is provenance only).
    let state = eligible_state();
    let mut record = record_for_content(NODE_FILE, "  trivial\n");
    record.snapshot_sha = "an-abandoned-line-sha".to_string();
    assert_eq!(
        preflight_claimed_attempt(&state, &cfg(), &record, Some(NODE_FILE)),
        SidecarPreflight::Proceed
    );
}

#[test]
fn drift_row_lastclean_rewind_content_mismatch_rejects() {
    let state = eligible_state();
    let record = record_for_content(NODE_FILE, "  trivial\n");
    let rewound = NODE_FILE.replace("True", "False → False");
    assert_eq!(
        preflight_claimed_attempt(&state, &cfg(), &record, Some(&rewound)),
        SidecarPreflight::Reject {
            reason: "stale_content".to_string()
        }
    );
}

#[test]
fn drift_row_cone_clean_reverted_content_rejects() {
    // Cone clean restored the node file to its theorem-stating
    // checkpoint (different bytes than the attempt's pre-image).
    let state = eligible_state();
    let record = record_for_content(NODE_FILE, "  trivial\n");
    let reverted = NODE_FILE.replace("  sorry", "  sorry -- restored");
    assert_eq!(
        preflight_claimed_attempt(&state, &cfg(), &record, Some(&reverted)),
        SidecarPreflight::Reject {
            reason: "stale_content".to_string()
        }
    );
}

#[test]
fn drift_row_ts_phase_gates_on_coverage_and_content() {
    // Stating-like phase: the primary may edit ANY node. Uncovered
    // targets ⇒ window shut ⇒ ineligible regardless of content.
    let mut state = eligible_state();
    state.phase = Phase::TheoremStating;
    state.configured_targets.insert(TargetId::from("t"));
    let record = record_for_content(NODE_FILE, "  trivial\n");
    assert_eq!(
        preflight_claimed_attempt(&state, &cfg(), &record, Some(NODE_FILE)),
        SidecarPreflight::Reject {
            reason: "ineligible".to_string()
        }
    );
    // Covered ⇒ open; a concurrent statement edit still dies on the
    // content gate.
    state
        .live
        .coverage
        .insert(TargetId::from("t"), [node("Rung")].into_iter().collect());
    assert_eq!(
        preflight_claimed_attempt(&state, &cfg(), &record, Some(NODE_FILE)),
        SidecarPreflight::Proceed
    );
    let edited = NODE_FILE.replace("True", "2 = 2");
    assert_eq!(
        preflight_claimed_attempt(&state, &cfg(), &record, Some(&edited)),
        SidecarPreflight::Reject {
            reason: "stale_content".to_string()
        }
    );
}

#[test]
fn drift_row_phase_advance_rejects_ineligible() {
    let mut state = eligible_state();
    state.phase = Phase::Cleanup;
    let record = record_for_content(NODE_FILE, "  trivial\n");
    assert_eq!(
        preflight_claimed_attempt(&state, &cfg(), &record, Some(NODE_FILE)),
        SidecarPreflight::Reject {
            reason: "ineligible".to_string()
        }
    );
}

#[test]
fn preflight_rejects_non_success_and_config_phase_toggle() {
    let state = eligible_state();
    let mut record = record_for_content(NODE_FILE, "  trivial\n");
    record.status = "failed".to_string();
    assert!(matches!(
        preflight_claimed_attempt(&state, &cfg(), &record, Some(NODE_FILE)),
        SidecarPreflight::Reject { reason } if reason.starts_with("not_success")
    ));

    // Operator toggled ProofFormalization off in the config block.
    let record = record_for_content(NODE_FILE, "  trivial\n");
    let cfg_off = SidecarRuntimeConfig {
        phases_proof_formalization: false,
        ..SidecarRuntimeConfig::default()
    };
    assert_eq!(
        preflight_claimed_attempt(&state, &cfg_off, &record, Some(NODE_FILE)),
        SidecarPreflight::Reject {
            reason: "ineligible".to_string()
        }
    );
}

// ====================================================================
// Queue redesign: membership + generation gates (amendment A3)
// ====================================================================

#[test]
fn preflight_rejects_unqueued_node_as_not_queued() {
    // Eligible but NOT queued: the reviewer-authority guard fires
    // between the eligibility recheck and the pre-image gate — a
    // daemon-misbehavior result for an unqueued node dies in
    // `rejected/not_queued`, worktree untouched.
    let mut state = eligible_state();
    state.sidecar_queue.clear();
    let record = record_for_content(NODE_FILE, "  trivial\n");
    assert_eq!(
        preflight_claimed_attempt(&state, &cfg(), &record, Some(NODE_FILE)),
        SidecarPreflight::Reject {
            reason: "not_queued".to_string()
        }
    );
}

#[test]
fn preflight_rejects_generation_mismatch_as_stale_generation() {
    // Remove + re-add minted a NEW generation (seq 8) while the attempt
    // was spawned for seq 7's predecessor... the publish/cancel race:
    // the stale-generation record must never land on the new entry.
    let mut state = eligible_state();
    state.sidecar_queue[0].entry_seq = 8;
    state.sidecar_queue_seq = 8;
    let record = record_for_content(NODE_FILE, "  trivial\n");
    assert!(matches!(
        preflight_claimed_attempt(&state, &cfg(), &record, Some(NODE_FILE)),
        SidecarPreflight::Reject { reason } if reason.starts_with("stale_generation")
    ));

    // A record MISSING entry_seq entirely (serde default 0) is stale by
    // construction — generations start at 1.
    let mut json: serde_json::Value =
        serde_json::from_str(&attempt_record_json(&sha256_hex(NODE_FILE.as_bytes()), "  t\n"))
            .unwrap();
    json.as_object_mut().unwrap().remove("entry_seq");
    let record = parse_attempt_record(&json.to_string()).unwrap();
    let state = eligible_state();
    assert!(matches!(
        preflight_claimed_attempt(&state, &cfg(), &record, Some(NODE_FILE)),
        SidecarPreflight::Reject { reason } if reason.starts_with("stale_generation")
    ));
}

#[test]
fn preflight_matching_generation_proceeds() {
    let state = eligible_state();
    let record = record_for_content(NODE_FILE, "  trivial\n");
    assert_eq!(record.entry_seq, 7, "fixture carries the queued generation");
    assert_eq!(
        preflight_claimed_attempt(&state, &cfg(), &record, Some(NODE_FILE)),
        SidecarPreflight::Proceed
    );
}

// ====================================================================
// A3 ban scan
// ====================================================================

#[test]
fn ban_scan_rejects_every_forbidden_and_extra_token() {
    // All 16 kernel keywords (incl. sorryAx first) ...
    for token in trellis_kernel::backend::LEAN_FORBIDDEN_KEYWORDS {
        let body = format!("  have h : True := trivial\n  {token} x\n");
        assert_eq!(
            sidecar_body_ban_scan(&body).as_deref(),
            Some(*token),
            "kernel keyword {token} must be caught"
        );
    }
    // ... plus the sidecar extras (A3: attribute / deriving / export
    // among them).
    for token in trellis_kernel::sidecar::SIDECAR_EXTRA_BANNED_TOKENS {
        let body = format!("  exact foo\n{token} [simp] Nat.add_zero\n");
        assert_eq!(
            sidecar_body_ban_scan(&body).as_deref(),
            Some(*token),
            "sidecar extra {token} must be caught"
        );
    }
    // Comments are NOT exempt (stricter than the primary masked scan).
    assert_eq!(
        sidecar_body_ban_scan("  trivial\n-- attribute [simp] foo\n").as_deref(),
        Some("attribute")
    );
    // Token boundaries: identifiers merely CONTAINING a banned token
    // pass.
    assert_eq!(sidecar_body_ban_scan("  exact attributeFoo x\n"), None);
    assert_eq!(sidecar_body_ban_scan("  exact my_export'\n"), None);
    assert_eq!(sidecar_body_ban_scan("  exact derivingLemma\n"), None);
    // A clean body passes.
    assert_eq!(
        sidecar_body_ban_scan("  intro h\n  simpa using h\n"),
        None
    );
}

// ====================================================================
// Only-BODY splice property (adversarial)
// ====================================================================

#[test]
fn splice_keeps_prefix_byte_frozen() {
    let spliced = splice_proof_body(NODE_FILE, "Rung", "  trivial\n").expect("splice");
    let split = trellis_kernel::filespec_split::split(NODE_FILE, "Rung").unwrap();
    assert_eq!(
        &spliced[..split.body_marker_end_byte],
        &NODE_FILE[..split.body_marker_end_byte],
        "prefix through the -- BODY line is byte-identical"
    );
    assert!(spliced.ends_with("  trivial\n"));
    assert!(validate_filespec(&spliced, "Rung").is_ok());
}

#[test]
fn splice_adversarial_body_with_smuggled_marker_dies_at_filespec() {
    let body = "  trivial\n-- BODY\n  sorry\n";
    let spliced = splice_proof_body(NODE_FILE, "Rung", body).expect("splice itself succeeds");
    let err = validate_filespec(&spliced, "Rung").expect_err("two markers must be rejected");
    assert!(err.contains("multiple"), "unexpected error: {err}");
}

#[test]
fn splice_adversarial_whole_file_emission_dies_at_filespec() {
    // Model emits an entire file (imports + marker + statement) as the
    // "body": the result carries two tablet markers → rejected.
    let spliced =
        splice_proof_body(NODE_FILE, "Rung", NODE_FILE).expect("splice itself succeeds");
    assert!(validate_filespec(&spliced, "Rung").is_err());
}

#[test]
fn splice_unicode_and_crlf_bodies_stay_below_the_marker() {
    let body = "  -- ∀ ε > 0 ∃ δ 🎯\r\n  trivial\r\n";
    let spliced = splice_proof_body(NODE_FILE, "Rung", body).expect("splice");
    let split = trellis_kernel::filespec_split::split(NODE_FILE, "Rung").unwrap();
    assert_eq!(&spliced[..split.body_marker_end_byte], &NODE_FILE[..split.body_marker_end_byte]);
    assert!(spliced.ends_with(body));
}

#[test]
fn splice_statement_region_never_moves_declaration_hash() {
    let repo = PathBuf::from("/nonexistent");
    let pre =
        trellis_kernel::filespec_split::declaration_hash_strict(&repo, NODE_FILE, "Rung").unwrap();
    let spliced = splice_proof_body(NODE_FILE, "Rung", "  exact True.intro\n").unwrap();
    let post =
        trellis_kernel::filespec_split::declaration_hash_strict(&repo, &spliced, "Rung").unwrap();
    assert_eq!(pre, post);
}

// ====================================================================
// Spool mechanics
// ====================================================================

#[test]
fn spool_claim_defer_ceiling_and_verdict_preserve_unknown_fields() {
    let dir = tempdir();
    let spool = spool_dirs(dir.path());
    ensure_spool_dirs(&spool).unwrap();

    let attempt = spool.pending.join("attempt-sc-1.json");
    std::fs::write(&attempt, attempt_record_json("sha", "  trivial\n")).unwrap();

    // Claim by rename.
    let pending = next_pending_attempt(&spool).expect("one pending");
    assert_eq!(pending, attempt);
    let claimed = claim_attempt(&spool, &pending).unwrap();
    assert!(!attempt.exists());
    assert!(claimed.exists());

    // First defer: back to pending with deferrals=1.
    let outcome = defer_attempt(&claimed, &spool, 7, "test").unwrap();
    assert_eq!(outcome, DeferOutcome::Deferred { deferrals: 1 });
    let re_pending = next_pending_attempt(&spool).expect("re-pending");
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&re_pending).unwrap()).unwrap();
    assert_eq!(value["deferrals"], serde_json::json!(1));
    // Unknown daemon fields survived the Value edit.
    assert_eq!(value["daemon_validation"]["compiled"], serde_json::json!(true));
    assert_eq!(value["provenance"]["tokens"]["prompt"], serde_json::json!(231044));

    // Second defer trips the D2 ceiling: rejected as apply_timeout.
    let claimed = claim_attempt(&spool, &re_pending).unwrap();
    let outcome = defer_attempt(&claimed, &spool, 8, "test").unwrap();
    assert_eq!(outcome, DeferOutcome::Rejected);
    assert!(next_pending_attempt(&spool).is_none());
    let rejected: Vec<_> = std::fs::read_dir(&spool.rejected)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(rejected.len(), 1);
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(rejected[0].path()).unwrap()).unwrap();
    assert_eq!(value["verdict"]["outcome"], serde_json::json!("rejected"));
    assert!(value["verdict"]["reason"]
        .as_str()
        .unwrap()
        .starts_with("apply_timeout"));
}

#[test]
fn spool_finalize_applied_appends_verdict() {
    let dir = tempdir();
    let spool = spool_dirs(dir.path());
    ensure_spool_dirs(&spool).unwrap();
    let claimed = spool.claimed.join("attempt-sc-2.json");
    std::fs::write(&claimed, attempt_record_json("sha", "  trivial\n")).unwrap();
    finalize_attempt(&claimed, &spool, "applied", "ok", 42, 811_400).unwrap();
    assert!(!claimed.exists());
    let applied = spool.applied.join("attempt-sc-2.json");
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&applied).unwrap()).unwrap();
    assert_eq!(value["verdict"]["outcome"], serde_json::json!("applied"));
    assert_eq!(value["verdict"]["cycle"], serde_json::json!(42));
    assert_eq!(value["attempt_id"], serde_json::json!("sc-20260722-213301-Rung"));
}

/// Amendment A4 — orphaned `claimed/*` swept back through the D2
/// mechanics: first orphan → pending with deferrals+1; an already-
/// deferred orphan → rejected (apply_timeout ceiling).
#[test]
fn journal_case_a4_orphaned_claims_swept_with_deferral_ceiling() {
    let dir = tempdir();
    let spool = spool_dirs(dir.path());
    ensure_spool_dirs(&spool).unwrap();
    std::fs::write(
        spool.claimed.join("attempt-fresh.json"),
        attempt_record_json("sha", "b"),
    )
    .unwrap();
    let mut deferred_once: serde_json::Value =
        serde_json::from_str(&attempt_record_json("sha", "b")).unwrap();
    deferred_once["deferrals"] = serde_json::json!(1);
    std::fs::write(
        spool.claimed.join("attempt-stale.json"),
        deferred_once.to_string(),
    )
    .unwrap();

    let swept = sweep_orphaned_claims(&spool, 9).unwrap();
    assert_eq!(swept.len(), 2);
    let outcomes: BTreeMap<String, DeferOutcome> = swept.into_iter().collect();
    assert_eq!(
        outcomes["attempt-fresh.json"],
        DeferOutcome::Deferred { deferrals: 1 }
    );
    assert_eq!(outcomes["attempt-stale.json"], DeferOutcome::Rejected);
    assert!(next_pending_attempt(&spool).is_some());
    assert!(spool.rejected.join("attempt-stale.json").exists());
    // Sweep over a nonexistent spool is a no-op, not an error.
    let empty = spool_dirs(&dir.path().join("elsewhere"));
    assert_eq!(sweep_orphaned_claims(&empty, 9).unwrap(), vec![]);
}

// ====================================================================
// Journal recovery — git-backed cases
// ====================================================================

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A git repo whose HEAD carries the canonical open node file.
fn seeded_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("Tablet")).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("Tablet/Rung.lean"), NODE_FILE).unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "seed"]);
    repo
}

fn journal_for(pre_image: &str) -> SidecarApplyJournal {
    SidecarApplyJournal {
        attempt_id: "sc-1".to_string(),
        node: node("Rung"),
        file: "Tablet/Rung.lean".to_string(),
        pre_image_sha: sha256_hex(pre_image.as_bytes()),
    }
}

/// Case 1 — crash after the worktree write, before any commit: the
/// dirty un-evented file is restored to HEAD and the journal cleared
/// (no silent `git add -A` sweep at the next checkpoint).
#[test]
fn journal_case_1_dirty_uncommitted_write_restored() {
    let dir = tempdir();
    let repo = seeded_repo(dir.path());
    let runtime_root = dir.path().join("runtime");
    std::fs::create_dir_all(&runtime_root).unwrap();
    write_apply_journal(&runtime_root, &journal_for(NODE_FILE)).unwrap();
    let spliced = splice_proof_body(NODE_FILE, "Rung", "  trivial\n").unwrap();
    std::fs::write(repo.join("Tablet/Rung.lean"), &spliced).unwrap();

    let outcome = run_journal_recovery(&runtime_root, &repo).unwrap();
    assert_eq!(
        outcome,
        JournalRecoveryOutcome::RestoredDirtyFile {
            restored_sha_matches_pre_image: true
        }
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("Tablet/Rung.lean")).unwrap(),
        NODE_FILE
    );
    // Journal cleared; second run is a no-op.
    assert_eq!(
        run_journal_recovery(&runtime_root, &repo).unwrap(),
        JournalRecoveryOutcome::NoJournal
    );
}

/// Case 2 — crash after the full apply (splice + commit + event):
/// the file matches HEAD, so recovery just clears the journal.
#[test]
fn journal_case_2_completed_apply_detected() {
    let dir = tempdir();
    let repo = seeded_repo(dir.path());
    let runtime_root = dir.path().join("runtime");
    std::fs::create_dir_all(&runtime_root).unwrap();
    write_apply_journal(&runtime_root, &journal_for(NODE_FILE)).unwrap();
    let spliced = splice_proof_body(NODE_FILE, "Rung", "  trivial\n").unwrap();
    std::fs::write(repo.join("Tablet/Rung.lean"), &spliced).unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "sidecar closure + event"]);

    let outcome = run_journal_recovery(&runtime_root, &repo).unwrap();
    assert_eq!(outcome, JournalRecoveryOutcome::ClearedFileMatchesHead);
    // Closure intact.
    assert_eq!(
        std::fs::read_to_string(repo.join("Tablet/Rung.lean")).unwrap(),
        spliced
    );
}

/// Case 3 — post-rewind HEAD mismatch: a LastClean rewind moved HEAD
/// under the journal (the file at HEAD differs from the recorded
/// pre-image). The dirty file is restored to CURRENT HEAD — the §1.4
/// refinement: the invariant is match-HEAD, never match-pre_image_sha
/// (which is logged as a diagnostic only).
#[test]
fn journal_case_3_post_rewind_head_mismatch_diagnostic() {
    let dir = tempdir();
    let repo = seeded_repo(dir.path());
    let runtime_root = dir.path().join("runtime");
    std::fs::create_dir_all(&runtime_root).unwrap();
    // Journal recorded against the ORIGINAL pre-image...
    write_apply_journal(&runtime_root, &journal_for(NODE_FILE)).unwrap();
    // ...but a rewind-like history change moved HEAD's copy.
    let rewound = NODE_FILE.replace("  sorry", "  sorry -- rewound line");
    std::fs::write(repo.join("Tablet/Rung.lean"), &rewound).unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "rewound baseline"]);
    // Crash left a dirty half-applied write on top.
    std::fs::write(
        repo.join("Tablet/Rung.lean"),
        splice_proof_body(&rewound, "Rung", "  trivial\n").unwrap(),
    )
    .unwrap();

    let outcome = run_journal_recovery(&runtime_root, &repo).unwrap();
    assert_eq!(
        outcome,
        JournalRecoveryOutcome::RestoredDirtyFile {
            restored_sha_matches_pre_image: false
        }
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("Tablet/Rung.lean")).unwrap(),
        rewound,
        "restore target is HEAD, not the journalled pre-image"
    );
}

/// Case 4 (amendment A7) — the commit-before-event-persist crash
/// window: the checkpoint sink committed the spliced file (HEAD has
/// the closure) but the crash hit before the event-log/state persist.
/// Recovery finds file-matches-HEAD and clears; the state-open /
/// disk-closed divergence then resolves via `open_nodes_from_repo`
/// (the node is sorry-free on disk) → the closed node lacks a record ⇒
/// `local_closure_unverified_nodes` ⇒ ONE synthesized re-verification.
/// The journal's job here is only "don't restore, don't error".
#[test]
fn journal_case_4_a7_commit_before_event_persist_window() {
    let dir = tempdir();
    let repo = seeded_repo(dir.path());
    let runtime_root = dir.path().join("runtime");
    std::fs::create_dir_all(&runtime_root).unwrap();
    write_apply_journal(&runtime_root, &journal_for(NODE_FILE)).unwrap();
    let spliced = splice_proof_body(NODE_FILE, "Rung", "  trivial\n").unwrap();
    std::fs::write(repo.join("Tablet/Rung.lean"), &spliced).unwrap();
    // The sink committed... (no event-log line — state persist never
    // ran; nothing in this fixture represents it, which is the point).
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "checkpoint commit only"]);

    let outcome = run_journal_recovery(&runtime_root, &repo).unwrap();
    assert_eq!(outcome, JournalRecoveryOutcome::ClearedFileMatchesHead);
    assert_eq!(
        std::fs::read_to_string(repo.join("Tablet/Rung.lean")).unwrap(),
        spliced,
        "the committed closure is kept — never rolled back by recovery"
    );
    // The state-side self-heal: a state that still lists the node open
    // reconciles from disk truth (sorry-free ⇒ not open ⇒ closed
    // without a record ⇒ unverified tier picks it up).
    let observed_open = trellis_kernel::open_nodes_from_repo(
        &repo,
        &[node("Rung")].into_iter().collect(),
    );
    assert!(
        !observed_open.contains(&node("Rung")),
        "disk truth: the node is closed; the unverified tier synthesizes one re-verification"
    );
}

/// Case 5 — no journal: recovery is a strict no-op (A5's unconditional
/// call sites rely on this being free).
#[test]
fn journal_case_5_no_journal_is_noop() {
    let dir = tempdir();
    let repo = seeded_repo(dir.path());
    let runtime_root = dir.path().join("runtime");
    std::fs::create_dir_all(&runtime_root).unwrap();
    assert_eq!(
        run_journal_recovery(&runtime_root, &repo).unwrap(),
        JournalRecoveryOutcome::NoJournal
    );
    assert!(
        !runtime_root.join("sidecar").exists(),
        "no-journal recovery must not create <runtime>/sidecar/"
    );
}

// ====================================================================
// Worker-repo mirror layout detection
// ====================================================================

#[test]
fn supervisor_workspace_layout_detection() {
    let dir = tempdir();
    let repo = dir.path().join("run-repo");
    // No workspace => no mirror target.
    std::fs::create_dir_all(repo.join("Tablet")).unwrap();
    assert_eq!(supervisor_workspace_tablet_dir(&repo), None);
    // Workspace present => the mirror target is its Tablet dir.
    let ws_tablet = repo.join(".trellis/supervisor/repo/Tablet");
    std::fs::create_dir_all(&ws_tablet).unwrap();
    assert_eq!(supervisor_workspace_tablet_dir(&repo), Some(ws_tablet.clone()));
    let wrote = trellis_kernel::sidecar::mirror_node_file_to_supervisor_workspace(
        &repo, "Rung", "new bytes",
    )
    .unwrap();
    assert!(wrote);
    assert_eq!(
        std::fs::read_to_string(ws_tablet.join("Rung.lean")).unwrap(),
        "new bytes"
    );
}

// ====================================================================
// Event determinism (replay pin at the engine tier)
// ====================================================================

#[test]
fn sidecar_event_replays_deterministically() {
    let state = {
        let mut state = eligible_state();
        state.stage = trellis_kernel::Stage::Start;
        state
    };
    let payload = trellis_kernel::SidecarClosurePayload {
        node: node("Rung"),
        attempt_id: "sc-1".to_string(),
        provider: "mistral".to_string(),
        model: "labs-leanstral-1-5".to_string(),
        wall_ms: 1,
        iterations: 1,
        declaration_hash_strict: "d".to_string(),
        node_file_sha256: "f".to_string(),
        record: trellis_kernel::LocalClosureRecord {
            node: node("Rung"),
            closure_version: "closure-v1".to_string(),
            kernel_axioms: ["propext".to_string()].into_iter().collect(),
            ..Default::default()
        },
    };
    let event = trellis_kernel::ProtocolEvent::SidecarClosure { payload };
    // Same state + same event ⇒ identical outcome, twice (the replay
    // property at the unit tier; the fixture-trace pin lands with the
    // spec commit).
    let a = trellis_kernel::apply_event(state.clone(), event.clone()).unwrap();
    let b = trellis_kernel::apply_event(state.clone(), event.clone()).unwrap();
    assert_eq!(a.state, b.state);
    assert_eq!(a.commands, b.commands);
    // And the wire form round-trips through the event-log record shape.
    let json = serde_json::to_string(&event).unwrap();
    let parsed: trellis_kernel::ProtocolEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, event);
}

// ====================================================================
// Outcome lane — spent-generation ingest surface
// ====================================================================

use trellis_kernel::sidecar::{
    classify_outcome, claim_outcome, finalize_outcome, nodes_awaiting_closure_ingest,
    parse_outcome_record, pending_outcome_files, return_outcome_to_lane,
    sweep_orphaned_outcome_claims, SidecarOutcomeDisposition,
};

fn outcome_json(node_name: &str, entry_seq: u64, status: &str, export_cycle: u32) -> String {
    serde_json::json!({
        "schema": 1,
        "attempt_id": format!("sc-{node_name}-{entry_seq}"),
        "node": node_name,
        "entry_seq": entry_seq,
        "status": status,
        "detail": "unsolved goals at line 12",
        "export_cycle": export_cycle,
        "ts": 1.0,
        "daemon_only_field": {"kept": true},
    })
    .to_string()
}

fn seed_outcome(spool: &trellis_kernel::sidecar::SidecarSpool, json: &str, name: &str) -> PathBuf {
    let path = spool.outcomes.join(name);
    std::fs::write(&path, json).expect("seed outcome");
    path
}

/// The pre-filter's happy path: a queued generation whose attempt is
/// spent expires.
#[test]
fn outcome_filter_expires_a_matching_generation() {
    let state = eligible_state();
    let record = parse_outcome_record(&outcome_json("Rung", 7, "failed", 0)).expect("parse");
    assert_eq!(
        classify_outcome(&state, &record, &Default::default()),
        SidecarOutcomeDisposition::Expire
    );
}

/// RISK 1's belt. A node whose closure is still sitting in `pending/`
/// (or mid-boundary in `claimed/`) must keep its queue entry — the
/// closure apply re-asserts membership and would die `not_queued`,
/// destroying completed grunt work. The outcome is not consumed
/// either; it goes back to the lane for the next boundary.
#[test]
fn outcome_for_node_with_closure_awaiting_ingest_is_requeued() {
    let state = eligible_state();
    let record = parse_outcome_record(&outcome_json("Rung", 7, "failed", 0)).expect("parse");
    let awaiting: std::collections::BTreeSet<NodeId> = [node("Rung")].into_iter().collect();
    assert_eq!(
        classify_outcome(&state, &record, &awaiting),
        SidecarOutcomeDisposition::Requeue
    );
}

/// The post-rewind gate: an outcome minted from an export cycle the
/// kernel has since rewound past describes a generation that no longer
/// exists.
#[test]
fn outcome_from_a_rewound_cycle_is_dropped() {
    let mut state = eligible_state();
    state.cycle = 40;
    let record = parse_outcome_record(&outcome_json("Rung", 7, "failed", 41)).expect("parse");
    assert_eq!(
        classify_outcome(&state, &record, &Default::default()),
        SidecarOutcomeDisposition::Drop {
            reason: "post_rewind".to_string()
        }
    );
    // Same cycle is fine — only a STRICTLY later export is impossible.
    let record = parse_outcome_record(&outcome_json("Rung", 7, "failed", 40)).expect("parse");
    assert_eq!(
        classify_outcome(&state, &record, &Default::default()),
        SidecarOutcomeDisposition::Expire
    );
}

#[test]
fn outcome_filter_drops_unqueued_and_stale_generations() {
    let mut state = eligible_state();
    state.sidecar_queue.clear();
    let record = parse_outcome_record(&outcome_json("Rung", 7, "failed", 0)).expect("parse");
    assert_eq!(
        classify_outcome(&state, &record, &Default::default()),
        SidecarOutcomeDisposition::Drop {
            reason: "not_queued".to_string()
        }
    );

    // Remove + re-add minted generation 8; the outcome for 7 is stale.
    let mut state = eligible_state();
    state.sidecar_queue[0].entry_seq = 8;
    assert_eq!(
        classify_outcome(&state, &record, &Default::default()),
        SidecarOutcomeDisposition::Drop {
            reason: "stale_generation".to_string()
        }
    );
}

/// A record missing `entry_seq` deserializes to generation 0, which is
/// never live (the counter starts at 1), so it can only no-op.
#[test]
fn outcome_without_entry_seq_cannot_match() {
    let state = eligible_state();
    let record = parse_outcome_record(
        r#"{"attempt_id":"sc-1","node":"Rung","status":"failed"}"#,
    )
    .expect("parse");
    assert_eq!(record.entry_seq, 0);
    assert_eq!(
        classify_outcome(&state, &record, &Default::default()),
        SidecarOutcomeDisposition::Drop {
            reason: "stale_generation".to_string()
        }
    );
}

/// Claim by rename, terminal move with a verdict, and unknown daemon
/// fields survive — the `pending/`-lane mechanics, re-pinned for the
/// outcome lane.
#[test]
fn outcome_claim_and_finalize_mechanics() {
    let dir = tempdir();
    let spool = spool_dirs(dir.path());
    ensure_spool_dirs(&spool).unwrap();
    seed_outcome(&spool, &outcome_json("Rung", 7, "failed", 3), "outcome-b.json");
    seed_outcome(&spool, &outcome_json("Ghost", 8, "crashed", 3), "outcome-a.json");

    let files = pending_outcome_files(&spool);
    assert_eq!(files.len(), 2);
    assert!(
        files[0].file_name().unwrap().to_string_lossy() == "outcome-a.json",
        "oldest-first by name"
    );

    let claimed = claim_outcome(&spool, &files[0]).expect("claim");
    assert!(claimed.starts_with(&spool.claimed_outcomes));
    assert!(!files[0].exists(), "claim is a rename out of the lane");

    let finalized = finalize_outcome(&claimed, &spool, "expired", 41).expect("finalize");
    assert!(finalized.starts_with(&spool.outcomes_consumed));
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&finalized).unwrap()).unwrap();
    assert_eq!(value["verdict"]["outcome"], serde_json::json!("expired"));
    assert_eq!(value["verdict"]["cycle"], serde_json::json!(41));
    assert_eq!(
        value["daemon_only_field"]["kept"],
        serde_json::json!(true),
        "unknown daemon fields survive the verdict edit"
    );
}

#[test]
fn returned_outcome_goes_back_to_the_lane_unconsumed() {
    let dir = tempdir();
    let spool = spool_dirs(dir.path());
    ensure_spool_dirs(&spool).unwrap();
    let seeded = seed_outcome(&spool, &outcome_json("Rung", 7, "failed", 3), "outcome-a.json");
    let before = std::fs::read_to_string(&seeded).unwrap();
    let claimed = claim_outcome(&spool, &seeded).expect("claim");
    let returned = return_outcome_to_lane(&claimed, &spool).expect("return");
    assert_eq!(returned, seeded);
    assert_eq!(
        std::fs::read_to_string(&returned).unwrap(),
        before,
        "returned byte-identically — no verdict, no consumption"
    );
    assert!(std::fs::read_dir(&spool.outcomes_consumed).unwrap().next().is_none());
}

/// Risk 8: the new lane carries the same crash-recovery discipline as
/// `pending/` — an orphaned claim returns to the lane once, and a
/// record that keeps orphaning is finalized rather than bouncing
/// forever.
#[test]
fn orphaned_outcome_claims_return_then_drop() {
    let dir = tempdir();
    let spool = spool_dirs(dir.path());
    ensure_spool_dirs(&spool).unwrap();
    std::fs::write(
        spool.claimed_outcomes.join("outcome-a.json"),
        outcome_json("Rung", 7, "failed", 3),
    )
    .unwrap();

    let swept = sweep_orphaned_outcome_claims(&spool, 12).expect("sweep");
    assert_eq!(swept, vec![("outcome-a.json".to_string(), true)]);
    assert!(spool.outcomes.join("outcome-a.json").exists());

    // Second orphaning of the same record: dropped with a verdict.
    let claimed = claim_outcome(&spool, &spool.outcomes.join("outcome-a.json")).unwrap();
    assert!(claimed.exists());
    let swept = sweep_orphaned_outcome_claims(&spool, 13).expect("sweep");
    assert_eq!(swept, vec![("outcome-a.json".to_string(), false)]);
    let value: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(spool.outcomes_consumed.join("outcome-a.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        value["verdict"]["outcome"],
        serde_json::json!("dropped:orphaned_claim")
    );
}

#[test]
fn sweep_orphaned_outcome_claims_is_inert_without_a_spool() {
    let dir = tempdir();
    let spool = spool_dirs(&dir.path().join("nothing-here"));
    assert!(sweep_orphaned_outcome_claims(&spool, 1).unwrap().is_empty());
    assert!(!spool.outcomes.exists(), "no directory creation");
}

/// The `closure_awaiting_ingest` input reads BOTH daemon->kernel
/// closure dirs, so a closure claimed mid-boundary still protects its
/// entry.
#[test]
fn nodes_awaiting_closure_ingest_reads_pending_and_claimed() {
    let dir = tempdir();
    let spool = spool_dirs(dir.path());
    ensure_spool_dirs(&spool).unwrap();
    std::fs::write(
        spool.pending.join("attempt-a.json"),
        attempt_record_json("sha", "  trivial\n"),
    )
    .unwrap();
    let mut claimed = serde_json::from_str::<serde_json::Value>(&attempt_record_json(
        "sha",
        "  trivial\n",
    ))
    .unwrap();
    claimed["node"] = serde_json::json!("Ghost");
    std::fs::write(
        spool.claimed.join("attempt-b.json"),
        serde_json::to_string(&claimed).unwrap(),
    )
    .unwrap();
    let nodes = nodes_awaiting_closure_ingest(&spool);
    assert_eq!(
        nodes,
        [node("Rung"), node("Ghost")].into_iter().collect::<std::collections::BTreeSet<_>>()
    );
}

/// The D2 ceiling, both halves, through the REAL defer mechanics.
///
/// An ordinary deferral puts the attempt back in `pending/` where it may
/// still land, so it contributes nothing and the queue entry survives —
/// expiring it there would throw away a live attempt. The ceiling
/// (`apply_timeout`) is terminal: the attempt is dead in `rejected/`,
/// and because the daemon wrote its `success` row before publishing,
/// no daemon outcome is ever coming for that generation. Without the
/// ceiling arm the entry would sit queued and SPENT forever — the same
/// failure this feature exists to remove, by a rarer path.
#[test]
fn defer_ceiling_expires_the_generation_but_a_plain_deferral_does_not() {
    use trellis_kernel::sidecar::{defer_attempt, defer_outcome_contribution};

    let dir = tempdir();
    let spool = spool_dirs(dir.path());
    ensure_spool_dirs(&spool).unwrap();
    let claimed = spool.claimed.join("attempt-sc-1.json");
    std::fs::write(&claimed, attempt_record_json("sha", "  trivial\n")).unwrap();
    let identity = Some((node("Rung"), 7u64, "sc-1".to_string()));

    // First defer: back to `pending/`, nothing spent.
    let first = defer_attempt(&claimed, &spool, 5, "observe_node transport").unwrap();
    assert_eq!(first, DeferOutcome::Deferred { deferrals: 1 });
    assert!(
        defer_outcome_contribution(&first, identity.clone(), "observe_node transport").is_none(),
        "a live attempt's generation is not spent"
    );
    // No contribution means no outcome enters the batch, so no event
    // names this generation and the entry stays queued for the attempt
    // that is still alive in `pending/`.
    let state = eligible_state();
    assert_eq!(state.sidecar_queue[0].entry_seq, 7);
    assert!(spool.pending.join("attempt-sc-1.json").exists());

    // Second defer of the same record: the ceiling rejects it terminally.
    let reclaimed = claim_attempt(&spool, &spool.pending.join("attempt-sc-1.json")).unwrap();
    let second = defer_attempt(&reclaimed, &spool, 6, "observe_node transport").unwrap();
    assert_eq!(second, DeferOutcome::Rejected);
    let outcome = defer_outcome_contribution(&second, identity, "observe_node transport")
        .expect("the ceiling spends the generation");
    assert_eq!(outcome.status, "rejected:defer_ceiling");
    assert_eq!(outcome.node.as_str(), "Rung");
    assert_eq!(outcome.entry_seq, 7);
    assert_eq!(
        outcome.source,
        trellis_kernel::SidecarAttemptOutcomeSource::KernelReject
    );
    assert!(outcome.detail.contains("apply_timeout"), "{}", outcome.detail);

    // And that outcome does expire the entry.
    let applied = trellis_kernel::apply_event(
        state,
        trellis_kernel::ProtocolEvent::SidecarAttemptOutcomes {
            payload: trellis_kernel::SidecarAttemptOutcomesPayload {
                outcomes: vec![outcome],
            },
        },
    )
    .expect("apply");
    assert!(applied.state.sidecar_queue.is_empty());
    assert_eq!(
        applied.state.sidecar_queue_prune_log[0].reason,
        "attempt_spent:rejected:defer_ceiling"
    );
}

/// An attempt that never named a live generation (unparseable record,
/// or one not claiming `success`) contributes nothing even at the
/// ceiling — there is no generation to spend.
#[test]
fn defer_ceiling_without_an_identity_contributes_nothing() {
    use trellis_kernel::sidecar::defer_outcome_contribution;

    assert!(defer_outcome_contribution(&DeferOutcome::Rejected, None, "ctx").is_none());
}

/// The LATE awaiting check, on the race it exists for.
///
/// `publish_attempt` runs in the attempt CHILD, so a closure reaches
/// `pending/` before the child's result file exists — and a
/// stop-sentinel cancel or a crash-streak burn publishes a spent-
/// generation report for that same generation. If the only awaiting
/// snapshot is the one taken at the top of the ingest, a closure that
/// lands during the claim loop is invisible, the entry is expired, and
/// the proof dies at `rejected/not_queued`. Success-never-publishes
/// does not help (no success was seen) and the generation match does
/// not either (the closure is for that exact generation), so this
/// second pass is the whole defense.
#[test]
fn outcome_is_withheld_when_a_closure_lands_during_ingest() {
    use trellis_kernel::sidecar::partition_batch_against_awaiting;

    let dir = tempdir();
    let spool = spool_dirs(dir.path());
    ensure_spool_dirs(&spool).unwrap();
    let seeded = seed_outcome(
        &spool,
        &outcome_json("Rung", 7, "cancelled", 0),
        "outcome-a.json",
    );
    let before = std::fs::read_to_string(&seeded).unwrap();

    // Snapshot 1 — the lane is quiet, so the record classifies Expire
    // and is claimed into the batch.
    let state = eligible_state();
    let awaiting_first = nodes_awaiting_closure_ingest(&spool);
    assert!(awaiting_first.is_empty());
    let record = parse_outcome_record(&before).expect("parse");
    assert_eq!(
        classify_outcome(&state, &record, &awaiting_first),
        SidecarOutcomeDisposition::Expire
    );
    let claimed = claim_outcome(&spool, &seeded).expect("claim");
    let batch = vec![(
        trellis_kernel::SidecarAttemptOutcome {
            node: node("Rung"),
            entry_seq: 7,
            attempt_id: "sc-1".to_string(),
            status: "cancelled".to_string(),
            detail: String::new(),
            source: trellis_kernel::SidecarAttemptOutcomeSource::Daemon,
        },
        Some(claimed.clone()),
    )];

    // The child wins the race: its closure lands in `pending/` while
    // the claim loop is still running.
    std::fs::write(
        spool.pending.join("attempt-child.json"),
        attempt_record_json("sha", "  trivial\n"),
    )
    .unwrap();

    // Snapshot 2, taken immediately before the step, sees it.
    let awaiting_now = nodes_awaiting_closure_ingest(&spool);
    assert!(awaiting_now.contains(&node("Rung")));
    let split = partition_batch_against_awaiting(batch, &awaiting_now);
    assert!(
        split.outcomes.is_empty(),
        "nothing may be applied for a node whose proof is arriving"
    );
    assert!(split.consumable.is_empty());
    assert_eq!(split.withheld, vec![(node("Rung"), Some(claimed.clone()))]);

    // Withheld means RETURNED, not consumed: the record goes back to
    // the lane byte-intact for the next boundary.
    return_outcome_to_lane(&claimed, &spool).expect("return");
    assert_eq!(std::fs::read_to_string(&seeded).unwrap(), before);
    assert!(std::fs::read_dir(&spool.outcomes_consumed)
        .unwrap()
        .next()
        .is_none());
}

/// Control for the above: with the lane still quiet at the late check,
/// the batch is carried and its file becomes consumable.
#[test]
fn late_awaiting_check_carries_the_batch_when_no_closure_arrives() {
    use trellis_kernel::sidecar::partition_batch_against_awaiting;

    let claimed = PathBuf::from("/tmp-unused/outcome-a.json");
    let batch = vec![
        (
            trellis_kernel::SidecarAttemptOutcome {
                node: node("Rung"),
                entry_seq: 7,
                attempt_id: "sc-1".to_string(),
                status: "failed".to_string(),
                detail: String::new(),
                source: trellis_kernel::SidecarAttemptOutcomeSource::Daemon,
            },
            Some(claimed.clone()),
        ),
        // A kernel rejection has no file: it is simply not carried when
        // withheld, and needs no return.
        (
            trellis_kernel::SidecarAttemptOutcome {
                node: node("Ghost"),
                entry_seq: 8,
                attempt_id: "sc-2".to_string(),
                status: "rejected:closure_probe".to_string(),
                detail: String::new(),
                source: trellis_kernel::SidecarAttemptOutcomeSource::KernelReject,
            },
            None,
        ),
    ];
    let split = partition_batch_against_awaiting(batch.clone(), &Default::default());
    assert_eq!(split.outcomes.len(), 2);
    assert_eq!(split.consumable, vec![claimed]);
    assert!(split.withheld.is_empty());

    let awaiting: std::collections::BTreeSet<NodeId> = [node("Ghost")].into_iter().collect();
    let split = partition_batch_against_awaiting(batch, &awaiting);
    assert_eq!(split.outcomes.len(), 1);
    assert_eq!(split.outcomes[0].node.as_str(), "Rung");
    assert_eq!(split.withheld, vec![(node("Ghost"), None)]);
}
