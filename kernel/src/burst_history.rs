//! Per-burst history ledger.
//!
//! Records one JSONL row per `WrapperResponse` processed by the kernel.
//! Intended audience: the reviewer/worker/verifier bursts that need a
//! cross-cycle view of recent activity — the per-burst kernel-authored
//! request_summary only includes the immediately-previous worker, so
//! sibling reviewer decisions on the same active node are invisible to
//! the current reviewer. The full event_log.jsonl has the data but is
//! gigabytes large and not agent-grep-friendly; this ledger is small,
//! line-oriented, and shape-stable for `tail` / `grep` access.
//!
//! File path: `<repo>/.trellis/logs/burst-history.jsonl`.
//! - Inside the bwrap sandbox (workers/reviewers `cwd` to the repo).
//! - Same dir as cost-ledger.jsonl, check-ledger.jsonl, tmux-backend-events.jsonl.
//!
//! Append-only, full history. The reviewer / worker prompt fragments
//! point agents at the file with the recommendation to `tail`. No
//! rotation; growth is bounded (~500 B/row, ~3000 bursts so far →
//! ~1.5 MB; ~30 K bursts in a long campaign → ~15 MB — still trivial
//! to `tail` / `grep`).
//!
//! Field truncation: a worker's `summary`/`comments` and a reviewer's
//! `comments` can be paragraph-length; truncate to keep rows scannable.
//! Full text lives in `<repo>/.trellis/chats/trellis_*_result/output.log`
//! for deep dives.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::model::{WrapperRequest, WrapperResponse};

static LOCK: Mutex<()> = Mutex::new(());

const MAX_TEXT: usize = 400;
const MAX_REASON: usize = 600;

fn ts_unix_seconds_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Absolute path to the burst-history ledger inside a repo.
pub fn history_path(repo_path: &Path) -> PathBuf {
    repo_path
        .join(".trellis")
        .join("logs")
        .join("burst-history.jsonl")
}

/// Truncate `s` to at most `max` characters (Unicode-safe), appending
/// `…` on truncation. Bytes-cap so very long lines don't bloat the log.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Build a JSON row from the dispatch-time request snapshot and the
/// just-received response. Captures fields useful for cross-cycle
/// pattern analysis (active_node, mode, must_close_active,
/// allow_new_obligations, outcome, deterministic_rejection_reasons,
/// transport_failure) without serializing the full state snapshot the
/// response carries.
pub fn build_row(request: &WrapperRequest, response: &WrapperResponse) -> Value {
    let mut row = Map::new();
    row.insert("ts".to_string(), json!(ts_unix_seconds_f64()));
    row.insert("cycle".to_string(), json!(request.cycle));
    row.insert("request_id".to_string(), json!(request.id));
    row.insert("kind".to_string(), json!(request.kind));
    row.insert("phase".to_string(), json!(request.phase));
    row.insert("mode".to_string(), json!(request.mode));
    row.insert("active_node".to_string(), json!(request.active_node));
    row.insert("held_target".to_string(), json!(request.held_target));
    row.insert(
        "retry_outcome_kind".to_string(),
        json!(request.retry_outcome_kind),
    );
    row.insert("retry_attempt".to_string(), json!(request.retry_attempt));
    row.insert(
        "invalid_attempt".to_string(),
        json!(request.invalid_attempt),
    );
    match response {
        WrapperResponse::Worker(r) => {
            row.insert("status".to_string(), json!(r.status));
            row.insert("outcome".to_string(), json!(r.outcome));
            row.insert("transport_failure".to_string(), json!(r.transport_failure));
            row.insert("summary".to_string(), json!(truncate(&r.summary, MAX_TEXT)));
            row.insert(
                "comments".to_string(),
                json!(truncate(&r.comments, MAX_TEXT)),
            );
            row.insert(
                "deterministic_rejection_reasons".to_string(),
                json!(r.deterministic_rejection_reasons),
            );
            row.insert(
                "added_deps_count".to_string(),
                json!(count_dep_set_updates(&r.dep_updates)),
            );
            row.insert(
                "proof_node_updates_count".to_string(),
                json!(r.proof_node_updates.len()),
            );
            row.insert(
                "difficulty_updates_count".to_string(),
                json!(r.difficulty_updates.len()),
            );
        }
        WrapperResponse::Review(r) => {
            row.insert("status".to_string(), json!(r.status));
            row.insert("decision".to_string(), json!(r.decision));
            row.insert("reset".to_string(), json!(r.reset));
            row.insert("next_active".to_string(), json!(r.next_active));
            row.insert("next_mode".to_string(), json!(r.next_mode));
            row.insert(
                "next_worker_context_mode".to_string(),
                json!(r.next_worker_context_mode),
            );
            row.insert("must_close_active".to_string(), json!(r.must_close_active));
            row.insert(
                "allow_new_obligations".to_string(),
                json!(r.allow_new_obligations),
            );
            row.insert("work_style_hint".to_string(), json!(r.work_style_hint));
            row.insert(
                "authorized_nodes_count".to_string(),
                json!(r.authorized_nodes.len()),
            );
            row.insert(
                "comments".to_string(),
                json!(truncate(&r.comments, MAX_REASON)),
            );
        }
        WrapperResponse::Paper(r) => {
            row.insert("status".to_string(), json!(r.status));
            row.insert(
                "target_lane_updates_count".to_string(),
                json!(r.target_lane_updates.len()),
            );
            row.insert(
                "node_lane_updates_count".to_string(),
                json!(r.node_lane_updates.len()),
            );
            row.insert(
                "reviewer_evidence_lanes".to_string(),
                json!(r.reviewer_evidence.len()),
            );
        }
        WrapperResponse::Corr(r) => {
            row.insert("status".to_string(), json!(r.status));
            row.insert(
                "node_lane_updates_count".to_string(),
                json!(r.node_lane_updates.len()),
            );
            row.insert(
                "target_lane_updates_count".to_string(),
                json!(r.target_lane_updates.len()),
            );
            row.insert(
                "reviewer_evidence_lanes".to_string(),
                json!(r.reviewer_evidence.len()),
            );
        }
        WrapperResponse::Sound(r) => {
            row.insert("status".to_string(), json!(r.status));
            row.insert(
                "lane_updates_count".to_string(),
                json!(r.lane_updates.len()),
            );
            row.insert(
                "reviewer_evidence_lanes".to_string(),
                json!(r.reviewer_evidence.len()),
            );
        }
        WrapperResponse::HumanGate(r) => {
            row.insert("status".to_string(), json!(r.status));
            row.insert("choice".to_string(), json!(r.choice));
        }
        WrapperResponse::Audit(r) => {
            row.insert("status".to_string(), json!(r.status));
            row.insert("audit_outcome".to_string(), json!(r.outcome));
            row.insert("new_tasks_count".to_string(), json!(r.new_tasks.len()));
            row.insert(
                "task_modifications_count".to_string(),
                json!(r.task_modifications.len()),
            );
            row.insert(
                "scratchpad_len".to_string(),
                json!(r.scratchpad_replace.len()),
            );
        }
        WrapperResponse::StuckMathAudit(r) => {
            row.insert("status".to_string(), json!(r.status));
            row.insert("report_len".to_string(), json!(r.report.chars().count()));
            row.insert("tasks_count".to_string(), json!(r.tasks.len()));
            row.insert("probe_paths_count".to_string(), json!(r.probe_paths.len()));
        }
    }
    Value::Object(row)
}

fn count_dep_set_updates<T: serde::Serialize>(updates: &T) -> usize {
    // dep_updates is BTreeMap<NodeId, Update<BTreeSet<NodeId>>>; we just
    // want the count of changed nodes for the row. Serialize once and
    // count the top-level object keys to stay decoupled from the exact
    // Update<> shape.
    match serde_json::to_value(updates) {
        Ok(Value::Object(map)) => map.len(),
        _ => 0,
    }
}

/// Append one row to the burst-history ledger. Best-effort: swallows
/// I/O errors so the kernel never fails to commit a wrapper response
/// just because telemetry is unavailable.
pub fn append(repo_path: &Path, request: &WrapperRequest, response: &WrapperResponse) {
    let path = history_path(repo_path);
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let row = build_row(request, response);
    let Ok(serialized) = serde_json::to_string(&row) else {
        return;
    };
    let _guard = LOCK.lock();
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let _ = writeln!(&mut file, "{}", serialized);
}

/// One-shot backfill from `event_log.jsonl`. Idempotent — does nothing
/// if `burst-history.jsonl` already exists. Best-effort — silently
/// returns on any I/O or parse failure so a malformed event_log can't
/// block startup.
///
/// Walks the event log line by line, looking for `wrapper_response`
/// events. For each one, reconstructs an approximate `request_id` /
/// `cycle` / response-kind row from the event payload (the event log
/// stores the full response object alongside the request id and
/// cycle, but does NOT store the dispatch-time request fields like
/// `active_node` / `mode` — we capture those from the response body
/// when present, otherwise leave them null).
pub fn backfill_if_missing(repo_path: &Path, event_log_path: &Path) {
    let history = history_path(repo_path);
    if history.exists() {
        return;
    }
    if !event_log_path.exists() {
        return;
    }
    let Some(parent) = history.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(file) = std::fs::File::open(event_log_path) else {
        return;
    };
    let reader = BufReader::with_capacity(1 << 20, file);
    let Ok(mut out) = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&history)
    else {
        // Another process won the race; do nothing.
        return;
    };
    let mut rows_written = 0usize;
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.is_empty() {
            continue;
        }
        let Ok(event_envelope): Result<Value, _> = serde_json::from_str(&line) else {
            continue;
        };
        // The event_log row shape: top-level has `event` which is itself
        // an object `{ event: "wrapper_response", response: { ... } }`.
        // We only care about wrapper_response rows.
        let Some(event) = event_envelope.get("event") else {
            continue;
        };
        let Some(event_kind) = event.get("event").and_then(|v| v.as_str()) else {
            continue;
        };
        if event_kind != "wrapper_response" {
            continue;
        }
        let Some(response_val) = event.get("response") else {
            continue;
        };
        let Some(response_kind) = response_val.get("kind").and_then(|v| v.as_str()) else {
            continue;
        };
        let mut row = Map::new();
        // Best-effort ts: event_log uses ts_ms (i64 ms); convert to seconds.
        if let Some(ts_ms) = event_envelope.get("ts_ms").and_then(|v| v.as_i64()) {
            row.insert("ts".to_string(), json!(ts_ms as f64 / 1000.0));
        } else {
            row.insert("ts".to_string(), json!(0.0));
        }
        row.insert(
            "cycle".to_string(),
            response_val
                .get("cycle")
                .cloned()
                .unwrap_or_else(|| event_envelope.get("cycle").cloned().unwrap_or(json!(null))),
        );
        row.insert(
            "request_id".to_string(),
            response_val
                .get("request_id")
                .cloned()
                .unwrap_or(json!(null)),
        );
        row.insert(
            "kind".to_string(),
            json!(snake_to_request_kind(response_kind)),
        );
        row.insert(
            "phase".to_string(),
            event_envelope.get("phase").cloned().unwrap_or(json!(null)),
        );
        // active_node / held_target / mode are NOT in the response body
        // for verifier kinds — only worker/review carry them indirectly.
        // For the backfill we leave these null when not extractable.
        row.insert(
            "active_node".to_string(),
            // The worker response includes a `snapshot.active_node`-like view but
            // not in this exact shape; we leave null for backfill to avoid wrong
            // attribution. Live appends DO capture this correctly.
            json!(null),
        );
        row.insert("backfilled".to_string(), json!(true));
        // Now extract response-kind-specific fields. We pull straight
        // from response_val to stay schema-stable.
        match response_kind {
            "worker" => {
                copy_if(&response_val, "status", &mut row);
                copy_if(&response_val, "outcome", &mut row);
                copy_if(&response_val, "transport_failure", &mut row);
                if let Some(s) = response_val.get("summary").and_then(|v| v.as_str()) {
                    row.insert("summary".to_string(), json!(truncate(s, MAX_TEXT)));
                }
                if let Some(s) = response_val.get("comments").and_then(|v| v.as_str()) {
                    row.insert("comments".to_string(), json!(truncate(s, MAX_TEXT)));
                }
                copy_if(&response_val, "deterministic_rejection_reasons", &mut row);
            }
            "review" => {
                copy_if(&response_val, "status", &mut row);
                copy_if(&response_val, "decision", &mut row);
                copy_if(&response_val, "reset", &mut row);
                copy_if(&response_val, "next_active", &mut row);
                copy_if(&response_val, "next_mode", &mut row);
                copy_if(&response_val, "next_worker_context_mode", &mut row);
                copy_if(&response_val, "must_close_active", &mut row);
                copy_if(&response_val, "allow_new_obligations", &mut row);
                copy_if(&response_val, "work_style_hint", &mut row);
                if let Some(s) = response_val.get("comments").and_then(|v| v.as_str()) {
                    row.insert("comments".to_string(), json!(truncate(s, MAX_REASON)));
                }
            }
            "paper" | "corr" | "sound" => {
                copy_if(&response_val, "status", &mut row);
            }
            "human_gate" => {
                copy_if(&response_val, "status", &mut row);
                copy_if(&response_val, "choice", &mut row);
            }
            _ => {}
        }
        let Ok(serialized) = serde_json::to_string(&Value::Object(row)) else {
            continue;
        };
        if writeln!(&mut out, "{}", serialized).is_err() {
            // I/O failure mid-backfill is bad but not fatal; leave a marker
            // and abort. Next startup will see the partial file as
            // "already exists" and skip — operator can inspect manually.
            let _ = writeln!(
                &mut out,
                "{{\"backfill_aborted\":true,\"reason\":\"write_error\",\"rows_written_before\":{}}}",
                rows_written
            );
            return;
        }
        rows_written += 1;
    }
    eprintln!(
        "trellis: burst-history backfill wrote {rows_written} row(s) to {}",
        history.display()
    );
}

fn copy_if(src: &Value, key: &str, dst: &mut Map<String, Value>) {
    if let Some(v) = src.get(key) {
        dst.insert(key.to_string(), v.clone());
    }
}

fn snake_to_request_kind(s: &str) -> &'static str {
    // Map response_kind (snake_case in event_log) to the same
    // RequestKind variant string the live-append path serializes.
    match s {
        "worker" => "Worker",
        "review" => "Review",
        "paper" => "Paper",
        "corr" => "Corr",
        "sound" => "Sound",
        "human_gate" => "HumanGate",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Phase, RequestKind, ResetChoice, ResponseStatus, ReviewDecisionKind, ReviewResponse,
        TaskMode, WorkerContextMode, WorkerOutcome, WorkerResponse, WorkerWorkStyleHint,
        WrapperRequest, WrapperResponse,
    };

    fn make_request() -> WrapperRequest {
        let mut req = WrapperRequest::default();
        req.id = 42;
        req.cycle = 7;
        req.kind = RequestKind::Worker;
        req.phase = Phase::ProofFormalization;
        req.mode = TaskMode::Local;
        req.active_node = Some("FooNode".into());
        req.retry_attempt = 1;
        req
    }

    #[test]
    fn build_row_worker_includes_outcome_and_truncates_text() {
        let req = make_request();
        let mut resp = WorkerResponse::default();
        resp.request_id = 42;
        resp.cycle = 7;
        resp.outcome = WorkerOutcome::Valid;
        resp.summary = "x".repeat(500);
        resp.comments = "ok".to_string();
        let row = build_row(&req, &WrapperResponse::Worker(resp));
        let row = row.as_object().unwrap();
        assert_eq!(row["kind"], json!("Worker"));
        assert_eq!(row["cycle"], json!(7));
        assert_eq!(row["request_id"], json!(42));
        assert_eq!(row["active_node"], json!("FooNode"));
        assert_eq!(row["outcome"], json!("Valid"));
        // truncated summary: 400 chars + the ellipsis
        let summary = row["summary"].as_str().unwrap();
        assert!(summary.ends_with('…'));
        assert_eq!(summary.chars().count(), 401);
        assert_eq!(row["comments"], json!("ok"));
    }

    #[test]
    fn build_row_review_includes_close_and_obligations_flags() {
        let mut req = make_request();
        req.kind = RequestKind::Review;
        let mut resp = ReviewResponse::default();
        resp.request_id = 42;
        resp.cycle = 7;
        resp.decision = ReviewDecisionKind::Continue;
        resp.must_close_active = false;
        resp.allow_new_obligations = true;
        resp.reset = ResetChoice::None;
        resp.next_mode = TaskMode::Restructure;
        resp.next_worker_context_mode = WorkerContextMode::Resume;
        resp.next_active = Some("FooNode".into());
        resp.work_style_hint = WorkerWorkStyleHint::None;
        resp.status = ResponseStatus::Ok;
        let row = build_row(&req, &WrapperResponse::Review(resp));
        let row = row.as_object().unwrap();
        assert_eq!(row["kind"], json!("Review"));
        assert_eq!(row["decision"], json!("Continue"));
        assert_eq!(row["must_close_active"], json!(false));
        assert_eq!(row["allow_new_obligations"], json!(true));
        assert_eq!(row["next_active"], json!("FooNode"));
        assert_eq!(row["next_mode"], json!("Restructure"));
    }

    #[test]
    fn truncate_short_string_is_passthrough() {
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn truncate_long_string_keeps_max_chars_plus_ellipsis() {
        let s = "abcdefghij";
        let out = truncate(s, 4);
        assert_eq!(out, "abcd…");
    }

    #[test]
    fn append_creates_file_and_writes_one_line() {
        let tmpdir = tempfile::tempdir().unwrap();
        let repo = tmpdir.path();
        std::fs::create_dir_all(repo.join(".trellis").join("logs")).unwrap();
        let req = make_request();
        let resp = WrapperResponse::Worker(WorkerResponse::default());
        append(repo, &req, &resp);
        let contents = std::fs::read_to_string(history_path(repo)).unwrap();
        assert_eq!(contents.lines().count(), 1);
        let row: Value = serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(row["kind"], json!("Worker"));
    }

    #[test]
    fn backfill_skips_if_history_exists() {
        let tmpdir = tempfile::tempdir().unwrap();
        let repo = tmpdir.path();
        std::fs::create_dir_all(repo.join(".trellis").join("logs")).unwrap();
        let history = history_path(repo);
        std::fs::write(&history, "preexisting\n").unwrap();
        let event_log = tmpdir.path().join("event_log.jsonl");
        // Fake event_log containing one wrapper_response.
        let event = json!({
            "cycle": 5,
            "phase": "ProofFormalization",
            "ts_ms": 1_700_000_000_000_i64,
            "event": {
                "event": "wrapper_response",
                "response": {
                    "kind": "review",
                    "request_id": 99,
                    "cycle": 5,
                    "decision": "Continue",
                    "must_close_active": false,
                    "allow_new_obligations": true,
                }
            }
        });
        std::fs::write(
            &event_log,
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        backfill_if_missing(repo, &event_log);
        // History file unchanged.
        assert_eq!(std::fs::read_to_string(&history).unwrap(), "preexisting\n");
    }

    #[test]
    fn backfill_runs_when_history_missing() {
        let tmpdir = tempfile::tempdir().unwrap();
        let repo = tmpdir.path();
        std::fs::create_dir_all(repo.join(".trellis").join("logs")).unwrap();
        let event_log = tmpdir.path().join("event_log.jsonl");
        let event = json!({
            "cycle": 5,
            "phase": "ProofFormalization",
            "ts_ms": 1_700_000_000_000_i64,
            "event": {
                "event": "wrapper_response",
                "response": {
                    "kind": "review",
                    "request_id": 99,
                    "cycle": 5,
                    "decision": "Continue",
                    "must_close_active": false,
                    "allow_new_obligations": true,
                    "comments": "hello",
                }
            }
        });
        std::fs::write(
            &event_log,
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        backfill_if_missing(repo, &event_log);
        let contents = std::fs::read_to_string(history_path(repo)).unwrap();
        let row: Value = serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(row["cycle"], json!(5));
        assert_eq!(row["kind"], json!("Review"));
        assert_eq!(row["must_close_active"], json!(false));
        assert_eq!(row["allow_new_obligations"], json!(true));
        assert_eq!(row["comments"], json!("hello"));
        assert_eq!(row["backfilled"], json!(true));
    }
}
