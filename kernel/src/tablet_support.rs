use crate::cache_key::lean_closure_cache_key_for_nodes;
use crate::disk_cache::{
    cache_dir_for_namespace, cache_lookup_dirs, disk_cache_get_first, disk_cache_put,
};
use crate::model::{NodeId, NodeKind};
use crate::tablet_root::{sync_tablet_root_from_repo, TabletRootSyncOutput};
use crate::worker_normalization::{
    direct_deps_from_repo, node_kinds_from_repo, open_nodes_from_repo, present_nodes_from_repo,
    tablet_target_for_repo,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// Disk-cache namespace for `materialize_tablet_oleans` outputs. Lives
/// at `<runtime_root>/checker-state/kernel-cache/materialize-oleans/`
/// when the supervisor exposes `TRELLIS_KERNEL_CACHE_ROOT`. See
/// `crate::disk_cache` for the file layout and pruning notes.
const MATERIALIZE_OLEANS_DISK_NAMESPACE: &str = "materialize-oleans";

/// Build the disk-cache lookup key for a node-set.
///
/// PATH-INDEPENDENT: the lookup key intentionally omits `canon_repo`.
/// See the same-named comment in `runtime_cli_observations` for the
/// rationale — different views of identical Lean content (supervisor
/// bwrap, live tablet, worker bwrap) had different paths, fragmenting
/// the cache. Correctness rests on the closure-content `value_key`
/// (currently carries `cache_v=2`; replay is keyed separately by the olean
/// attestation rather than by making source-current artifacts stale).
///
/// `_canon_repo` is kept in the signature for call-site stability.
fn materialize_oleans_disk_lookup_key(_canon_repo: &Path, cleaned_nodes: &[String]) -> String {
    let mut key = String::new();
    for node in cleaned_nodes {
        key.push_str("node=");
        key.push_str(node);
        key.push('\n');
    }
    key
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabletSupportObservation {
    pub updated_paths: Vec<String>,
    pub header_tex_path: String,
    pub index_md_path: String,
    pub readme_md_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabletSupportRenderOutput {
    pub header_tex_path: String,
    pub header_tex_content: Option<String>,
    pub index_md_path: String,
    pub index_md_content: String,
    pub readme_md_path: String,
    pub readme_md_content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabletSupportSyncOutput {
    pub root: TabletRootSyncOutput,
    pub support: TabletSupportObservation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorWorkspaceSyncOutput {
    pub authoritative_repo_path: String,
    pub supervisor_home: String,
    pub supervisor_cache: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabletSupportNodeSnapshot {
    pub name: NodeId,
    pub env: String,
    pub kind: String,
    pub status: String,
    pub title: String,
    pub refs: Vec<String>,
    pub imports: Vec<NodeId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabletSupportMetricsSnapshot {
    pub total_nodes: usize,
    pub closed_nodes: usize,
    pub open_nodes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabletSupportSnapshot {
    pub nodes: Vec<TabletSupportNodeSnapshot>,
    pub metrics: TabletSupportMetricsSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ExternalCommandObservation {
    pub returncode: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub spawn_error: String,
}

fn repo_check_script_path(repo_path: &Path) -> PathBuf {
    repo_path.join(".trellis").join("scripts").join("check.py")
}

const PREAMBLE_NODE: &str = "Preamble";
const TABLET_INDEX_FILENAME: &str = "INDEX.md";
const TABLET_README_FILENAME: &str = "README.md";
const TABLET_HEADER_FILENAME: &str = "header.tex";

/// Tablet support files whose contents are unconditionally regenerated from
/// the current node tree by every Lean support render.
///
/// `header.tex` is deliberately absent: the renderer creates it only when it
/// does not exist and preserves an existing file byte-for-byte. Likewise,
/// `Preamble.tex` is structural node content, not a render output. Scope
/// checks may ignore modifications to the two derived files below, but must
/// continue to attribute edits to those other structural files to the worker.
pub fn is_kernel_regenerated_tablet_support_filename(name: &str) -> bool {
    matches!(name, TABLET_INDEX_FILENAME | TABLET_README_FILENAME)
}

fn node_tex_path(repo_path: &Path, node: &str) -> PathBuf {
    repo_path.join("Tablet").join(format!("{node}.tex"))
}

fn tablet_dir(repo_path: &Path) -> PathBuf {
    repo_path.join("Tablet")
}

fn index_md_path(repo_path: &Path) -> PathBuf {
    tablet_dir(repo_path).join(TABLET_INDEX_FILENAME)
}

fn readme_md_path(repo_path: &Path) -> PathBuf {
    tablet_dir(repo_path).join(TABLET_README_FILENAME)
}

fn header_tex_path(repo_path: &Path) -> PathBuf {
    tablet_dir(repo_path).join(TABLET_HEADER_FILENAME)
}

fn read_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn support_node_kind(kind: Option<&NodeKind>) -> &'static str {
    match kind {
        Some(NodeKind::Preamble) => "preamble",
        Some(NodeKind::Proof) => "proof",
        Some(NodeKind::Definition) => "definition",
        None => "definition",
    }
}

fn statement_meta_from_repo(repo_path: &Path, node: &str) -> (String, String, Vec<String>) {
    if node == PREAMBLE_NODE {
        return ("preamble".to_string(), String::new(), Vec::new());
    }
    let blocks =
        crate::extract_paper_statement_blocks(&read_text(&node_tex_path(repo_path, node)), None);
    if let Some(block) = blocks.into_iter().next() {
        return (block.env, block.title, block.labels);
    }
    (String::new(), String::new(), Vec::new())
}

pub fn build_tablet_support_snapshot_from_repo(
    repo_path: &Path,
) -> Result<TabletSupportSnapshot, String> {
    let present_nodes = present_nodes_from_repo(repo_path)?;
    let open_nodes = open_nodes_from_repo(repo_path, &present_nodes);
    let node_kinds = node_kinds_from_repo(repo_path, &present_nodes);
    let deps = direct_deps_from_repo(repo_path, &present_nodes);

    let nodes = present_nodes
        .iter()
        .map(|node| {
            let (env, title, refs) = statement_meta_from_repo(repo_path, node);
            let imports = deps
                .get(node)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect::<Vec<NodeId>>();
            TabletSupportNodeSnapshot {
                name: node.clone(),
                env: if env.trim().is_empty() {
                    "-".to_string()
                } else {
                    env
                },
                kind: support_node_kind(node_kinds.get(node)).to_string(),
                status: if open_nodes.contains(node) {
                    "open".to_string()
                } else {
                    "closed".to_string()
                },
                title,
                refs,
                imports,
            }
        })
        .collect::<Vec<_>>();

    let total_nodes = nodes.iter().filter(|node| node.kind != "preamble").count();
    let closed_nodes = nodes
        .iter()
        .filter(|node| node.kind != "preamble" && node.status == "closed")
        .count();
    let open_nodes_count = nodes
        .iter()
        .filter(|node| node.kind != "preamble" && node.status == "open")
        .count();

    Ok(TabletSupportSnapshot {
        nodes,
        metrics: TabletSupportMetricsSnapshot {
            total_nodes,
            closed_nodes,
            open_nodes: open_nodes_count,
        },
    })
}

fn generate_index_md(snapshot: &TabletSupportSnapshot) -> String {
    let mut lines = vec![
        "# Tablet Index".to_string(),
        String::new(),
        "| Name | Env | Kind | Status | Labels | Title | Imports |".to_string(),
        "|------|-----|------|--------|--------|-------|---------|".to_string(),
    ];
    for node in &snapshot.nodes {
        let imports = if node.imports.is_empty() {
            "-".to_string()
        } else {
            node.imports.join(", ")
        };
        let refs = if node.refs.is_empty() {
            "-".to_string()
        } else {
            node.refs.join(", ")
        };
        let title = if node.title.trim().is_empty() {
            "-".to_string()
        } else {
            node.title.clone()
        };
        lines.push(format!(
            "| {} | {} | {} | {} | {} | {} | {} |",
            node.name, node.env, node.kind, node.status, refs, title, imports
        ));
    }
    lines.push(String::new());
    lines.push(format!(
        "**Total:** {} nodes | **Closed:** {} | **Open:** {}",
        snapshot.metrics.total_nodes, snapshot.metrics.closed_nodes, snapshot.metrics.open_nodes
    ));
    lines.push(String::new());
    lines.join("\n")
}

fn generate_readme_md(snapshot: &TabletSupportSnapshot) -> String {
    let labeled_nodes: Vec<&TabletSupportNodeSnapshot> = snapshot
        .nodes
        .iter()
        .filter(|node| node.kind != "preamble" && !node.refs.is_empty())
        .collect();
    let unlabeled_nodes: Vec<&TabletSupportNodeSnapshot> = snapshot
        .nodes
        .iter()
        .filter(|node| node.kind != "preamble" && node.refs.is_empty())
        .collect();
    let mut lines = vec!["# Proof Tablet".to_string(), String::new()];
    if !labeled_nodes.is_empty() {
        lines.push("## Nodes With Labels".to_string());
        lines.push(String::new());
        lines.push("| Name | Labels | Title | Status |".to_string());
        lines.push("|------|--------|-------|--------|".to_string());
        for node in labeled_nodes {
            lines.push(format!(
                "| {} | {} | {} | {} |",
                node.name,
                node.refs.join(", "),
                if node.title.trim().is_empty() {
                    "-".to_string()
                } else {
                    node.title.clone()
                },
                node.status
            ));
        }
        lines.push(String::new());
    }
    if !unlabeled_nodes.is_empty() {
        lines.push("## Nodes Without Labels".to_string());
        lines.push(String::new());
        lines.push("| Name | Kind | Title | Status |".to_string());
        lines.push("|------|------|-------|--------|".to_string());
        for node in unlabeled_nodes {
            lines.push(format!(
                "| {} | {} | {} | {} |",
                node.name,
                node.kind,
                if node.title.trim().is_empty() {
                    "-".to_string()
                } else {
                    node.title.clone()
                },
                node.status
            ));
        }
        lines.push(String::new());
    }
    lines.push(format!(
        "**Summary:** {}/{} closed",
        snapshot.metrics.closed_nodes, snapshot.metrics.total_nodes
    ));
    lines.push(String::new());
    lines.join("\n")
}

fn generate_header_tex() -> String {
    "% Tablet LaTeX header -- generated by .trellis\n\
% Do not edit manually.\n\
\n\
\\newcommand{\\noderef}[1]{\\texttt{#1}}\n"
        .to_string()
}

pub fn build_tablet_support_render_output(
    repo_path: &Path,
    snapshot: &TabletSupportSnapshot,
) -> TabletSupportRenderOutput {
    let header_path = header_tex_path(repo_path);
    TabletSupportRenderOutput {
        header_tex_path: header_path.display().to_string(),
        header_tex_content: if header_path.exists() {
            None
        } else {
            Some(generate_header_tex())
        },
        index_md_path: index_md_path(repo_path).display().to_string(),
        index_md_content: generate_index_md(snapshot),
        readme_md_path: readme_md_path(repo_path).display().to_string(),
        readme_md_content: generate_readme_md(snapshot),
    }
}

fn run_repo_command_json(
    repo_path: &Path,
    subcommand: &str,
    args: &[String],
) -> Result<serde_json::Value, String> {
    run_repo_command_json_with_stdin(repo_path, subcommand, args, None)
}

fn run_repo_command_json_with_stdin(
    repo_path: &Path,
    subcommand: &str,
    args: &[String],
    stdin_payload: Option<&str>,
) -> Result<serde_json::Value, String> {
    use std::io::Write;
    let start = std::time::Instant::now();
    let mut command = Command::new("python3");
    command
        .arg(repo_check_script_path(repo_path))
        .arg(subcommand)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin_payload.is_some() {
        command.stdin(Stdio::piped());
    }
    let spawn_result = command.spawn();
    let output = match spawn_result {
        Ok(mut child) => {
            if let Some(payload) = stdin_payload {
                let write_result = match child.stdin.take() {
                    Some(mut stdin) => stdin.write_all(payload.as_bytes()),
                    None => Ok(()),
                };
                if let Err(err) = write_result {
                    let duration = start.elapsed().as_secs_f64();
                    // Don't kill immediately: when EPIPE fires, the child has
                    // already exited (which is why the pipe is broken).
                    // `wait_with_output` collects whatever stdout/stderr the
                    // child wrote before exiting — typically a Python
                    // traceback that pinpoints WHY it exited. Killing first
                    // throws that diagnostic away. Bounded by a brief wait
                    // because the child is already gone; if for some reason
                    // it isn't, fall back to killing after the wait returns.
                    let captured = child.wait_with_output();
                    let stderr_excerpt = match captured {
                        Ok(out) => {
                            let stderr_text = String::from_utf8_lossy(&out.stderr);
                            let stdout_text = String::from_utf8_lossy(&out.stdout);
                            let combined = if stderr_text.trim().is_empty() {
                                stdout_text.trim().to_string()
                            } else {
                                stderr_text.trim().to_string()
                            };
                            // Cap the captured excerpt so a runaway child
                            // can't blow up the error message.
                            if combined.len() > 2000 {
                                format!("{}…", &combined[..2000])
                            } else {
                                combined
                            }
                        }
                        Err(wait_err) => format!("<wait_with_output failed: {wait_err}>"),
                    };
                    crate::check_ledger::append(repo_path, subcommand, duration, false, 0, 0);
                    return Err(format!(
                        "write stdin to {subcommand} failed: {err}; child output: {stderr_excerpt}"
                    ));
                }
            }
            child.wait_with_output()
        }
        Err(err) => {
            let duration = start.elapsed().as_secs_f64();
            crate::check_ledger::append(repo_path, subcommand, duration, false, 0, 0);
            return Err(format!("spawn {subcommand} failed: {err}"));
        }
    };
    let duration = start.elapsed().as_secs_f64();
    let output = match output {
        Ok(o) => o,
        Err(err) => {
            crate::check_ledger::append(repo_path, subcommand, duration, false, 0, 0);
            return Err(format!("spawn {subcommand} failed: {err}"));
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let parsed = serde_json::from_str::<serde_json::Value>(stdout.trim());
    crate::check_ledger::append(
        repo_path,
        subcommand,
        duration,
        parsed.is_ok() && output.status.success(),
        stdout.len(),
        stderr.len(),
    );
    parsed.map_err(|err| {
        format!(
            "{subcommand} returned invalid JSON: {err}; stdout={:?}; stderr={:?}",
            stdout.trim(),
            stderr.trim()
        )
    })
}

pub fn sync_tablet_render_support_from_repo(
    repo_path: &Path,
) -> Result<TabletSupportObservation, String> {
    let driver = crate::backend::checker_driver_for(tablet_target_for_repo(repo_path));
    match driver {
        // Lean: render INDEX/README/header on the kernel side and pipe the
        // payload via stdin (it can exceed ARG_MAX with ~420 nodes). Byte-
        // identical to the historical hardcoded-Lean construction — the
        // dispatch AND the response parse (`TabletSupportObservation`, with
        // its required `updated_paths`) are unchanged.
        crate::backend::CheckerDriver::Lean(_) => {
            let support_snapshot = build_tablet_support_snapshot_from_repo(repo_path)?;
            let render_output = build_tablet_support_render_output(repo_path, &support_snapshot);
            let render_json = serde_json::to_string(&render_output)
                .map_err(|err| format!("serialize tablet support render failed: {err}"))?;
            let raw = run_repo_command_json_with_stdin(
                repo_path,
                driver.sync_tablet_support_op(),
                &[
                    repo_path.display().to_string(),
                    "--render-json".to_string(),
                    "-".to_string(),
                ],
                Some(&render_json),
            )?;
            serde_json::from_value(raw).map_err(|err| {
                format!("parse {} output failed: {err}", driver.sync_tablet_support_op())
            })
        }
        // Isabelle: the workspace render is socket-only — the server's
        // `_isabelle_sync_session` handler derives the session dir from its
        // own socket runtime root and writes the `ROOT`/`Tablet_Preamble`/
        // node scaffold itself (B2d). It consumes NO stdin and has NO
        // `--render-json` arg, so the kernel sends ONLY `[repo]` and pipes
        // nothing; sending the Lean render shape would be a parser error.
        //
        // RESPONSE SHAPE — unlike the Lean `sync-tablet-support` (which
        // returns the render-tracking `TabletSupportObservation` with
        // `updated_paths`), the `_isabelle_sync_session` handler returns the
        // EXTERNAL-COMMAND ENVELOPE (`isabelle_observations.external_command_
        // envelope`: `{returncode, stdout, stderr, timed_out, spawn_error}`),
        // with the scaffold summary JSON-encoded inside `stdout` and
        // `returncode: null` on a write failure (fail-closed). This is the
        // same shape every other Isabelle op uses (`isabelle-check-node` /
        // `-build-session` / `-thm-oracles` / `-thm-deps`). Parse it as such
        // and gate it through `ensure_external_command_ok` so a `null`/non-
        // zero `returncode`, a timeout, or a spawn error fails the precondition
        // hard — exactly how `materialize_tablet_oleans` / `prepare_compiled_
        // support` consume `isabelle-build-session`.
        //
        // The kernel does not track Isabelle's `updated_paths` (the server
        // owns the scaffold render), so on success we synthesize a
        // `TabletSupportObservation` with an empty `updated_paths` and the
        // canonical Tablet render paths; downstream consumers propagate the
        // observation but read none of these render fields for Isabelle.
        crate::backend::CheckerDriver::IsabelleHol(_) => {
            let raw = run_repo_command_json(
                repo_path,
                driver.sync_tablet_support_op(),
                &[repo_path.display().to_string()],
            )?;
            let observation: ExternalCommandObservation =
                serde_json::from_value(raw).map_err(|err| {
                    format!("parse {} output failed: {err}", driver.sync_tablet_support_op())
                })?;
            ensure_external_command_ok(driver.sync_tablet_support_op(), &observation)?;
            Ok(TabletSupportObservation {
                updated_paths: Vec::new(),
                header_tex_path: header_tex_path(repo_path).display().to_string(),
                index_md_path: index_md_path(repo_path).display().to_string(),
                readme_md_path: readme_md_path(repo_path).display().to_string(),
            })
        }
    }
}

pub fn sync_tablet_support_from_repo(repo_path: &Path) -> Result<TabletSupportSyncOutput, String> {
    let support = sync_tablet_render_support_from_repo(repo_path)?;
    let root = sync_tablet_root_from_repo(repo_path)?;
    Ok(TabletSupportSyncOutput { root, support })
}

pub fn sync_supervisor_workspace_from_repo(
    repo_path: &Path,
) -> Result<SupervisorWorkspaceSyncOutput, String> {
    let raw = match run_repo_command_json(
        repo_path,
        "sync-supervisor-workspace",
        &[repo_path.display().to_string()],
    ) {
        Ok(raw) => raw,
        Err(err)
            if err.contains("unexpected command: sync-supervisor-workspace")
                || err.contains("unexpected subcommand: sync-supervisor-workspace") =>
        {
            return Ok(SupervisorWorkspaceSyncOutput {
                authoritative_repo_path: repo_path.display().to_string(),
                supervisor_home: String::new(),
                supervisor_cache: String::new(),
            });
        }
        Err(err) => return Err(err),
    };
    serde_json::from_value(raw)
        .map_err(|err| format!("parse sync-supervisor-workspace output failed: {err}"))
}

/// Process-local short-circuit cache for `materialize-tablet-oleans`
/// dispatches.
///
/// `materialize-tablet-oleans` is the heaviest single op in a typical
/// fingerprint walk: live runs show ~17 minutes per cycle on a ~90-node
/// closure, dwarfing the per-node `lean-compile-node` cost. Even a no-op
/// "everything is already built" call still has to walk the closure,
/// inspect every olean, and pay lake-lock overhead.
///
/// Same correctness contract as the binary-side caches (see
/// `runtime_cli_observations.rs`'s `COMPILE_NODE_CACHE` /
/// `LEAN_SEMANTIC_PAYLOAD_CACHE`):
///
///   * **Pure content hashing.** Cache key is the content-hash blob built
///     by `lean_closure_cache_key_for_nodes` — lake state + check.py +
///     `Preamble.lean` + each requested node's per-node hash (which itself
///     covers that node's own .lean + its transitive imports).
///   * **Olean-presence guard.** Even on key match, every requested node's
///     `Tablet/<node>.olean` must exist on disk. A worker-hygiene cleanup,
///     manual rm, or `.lake` wipe between cached observations would
///     otherwise leave the cached "build succeeded" answer pinned while
///     the artefact is gone — the next op that needs the olean
///     (`lean-compile-node`, `lean-semantic-payloads`) would fail. The
///     guard forces a fresh dispatch in that case to rebuild.
///   * **Conservative on failure.** Only memoised when `returncode ==
///     Some(0)`, no spawn error, no timeout. A transient failure must
///     not be pinned (the next call may succeed once the worker
///     recovers).
///   * **Process-local; no persistence.** Bounded by the number of
///     distinct (canonical_repo, node-set, content-hash) tuples observed
///     in this kernel-binary process.
type MaterializeOleansCacheKey = String;
static MATERIALIZE_OLEANS_CACHE: LazyLock<
    Mutex<HashMap<(PathBuf, Vec<String>), (MaterializeOleansCacheKey, ExternalCommandObservation)>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Olean-staleness + replay gate (end-to-end): an olean is reusable iff its
/// unified provenance sidecar carries both the current source-closure hash and
/// a replay attestation for the exact olean bytes/current toolchain.
///
/// This hardens the materialize-tablet-oleans cache hit guard: the cache key
/// (`lean_closure_cache_key_for_nodes`) already misses on source-content
/// drift, but the *guard* was existence-only — so a persisted disk-cache
/// entry plus a present-but-content-stale olean (e.g. a `sorry` olean frozen
/// mtime-newer than a re-derived source after a rewind round-trip) could serve
/// a cached "materialized" answer while the on-disk olean is stale. Comparing
/// the sidecar against the recomputed key closes that gap. Legacy bare-hash
/// sidecars remain source-readable by Python so deployment
/// preserves their oleans, but they deliberately fail this trusted cache
/// guard and force one materialize/replay migration. Missing/unreadable
/// records or any mismatch fail closed.
fn olean_content_current(repo_path: &Path, node: &str) -> bool {
    let exported = repo_path
        .join(".lake/build/lib/lean/Tablet")
        .join(format!("{node}.olean"));
    let exported_bytes = match fs::read(&exported) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        _ => return false,
    };
    let server = PathBuf::from(format!("{}.server", exported.display()));
    let private = PathBuf::from(format!("{}.private", exported.display()));
    if private.exists() && !server.exists() {
        return false;
    }
    let mut artifact_files = vec![("exported", exported, exported_bytes)];
    if server.exists() {
        let server_bytes = match fs::read(&server) {
            Ok(bytes) if !bytes.is_empty() => bytes,
            _ => return false,
        };
        artifact_files.push(("server", server, server_bytes));
        if private.exists() {
            let private_bytes = match fs::read(&private) {
                Ok(bytes) if !bytes.is_empty() => bytes,
                _ => return false,
            };
            artifact_files.push(("private", private, private_bytes));
        }
    }
    let artifact_bundle: Vec<serde_json::Value> = artifact_files
        .iter()
        .map(|(level, _, bytes)| {
            serde_json::json!({
                "level": level,
                "sha256": crate::cache_key::hash_bytes(bytes),
                "size_bytes": bytes.len(),
            })
        })
        .collect();
    let current = match crate::cache_key::lean_closure_cache_key(repo_path, node) {
        Some(key) => key,
        None => return false,
    };
    let sidecar = repo_path
        .join(".lake/build/lib/lean/Tablet")
        .join(format!("{node}.olean.srcclosure"));
    let stored = match fs::read_to_string(&sidecar) {
        Ok(stored) => stored,
        Err(_) => return false,
    };
    let record: serde_json::Value = match serde_json::from_str(stored.trim()) {
        Ok(record) => record,
        // Pre-attestation bare source hash: source-current, replay-cold.
        Err(_) => return false,
    };
    if record.get("schema_version").and_then(|v| v.as_u64()) != Some(3)
        || record
            .get("source_closure_sha256")
            .and_then(|v| v.as_str())
            != Some(current.as_str())
    {
        return false;
    }
    let Some(replay) = record.get("kernel_replay") else {
        return false;
    };
    let toolchain_bytes = fs::read(repo_path.join("lean-toolchain")).unwrap_or_default();
    let toolchain_sha256 = crate::cache_key::hash_bytes(&toolchain_bytes);
    let declaration_manifest_is_valid = replay
        .get("declaration_manifest")
        .and_then(|value| value.as_array())
        .is_some_and(|manifest| {
            manifest.iter().all(|entry| {
                entry
                    .get("name")
                    .and_then(|value| value.as_str())
                    .is_some_and(|name| !name.is_empty())
                    && entry
                        .get("kind")
                        .and_then(|value| value.as_str())
                        .is_some_and(|kind| !kind.is_empty())
            })
        });
    let visibility_is_valid = replay
        .get("visibility_manifests")
        .and_then(|value| value.as_array())
        .is_some_and(|visibility| {
            visibility.len() == artifact_bundle.len()
                && visibility.iter().zip(&artifact_bundle).all(|(manifest, part)| {
                    manifest.get("level") == part.get("level")
                        && manifest
                            .get("declarations")
                            .is_some_and(serde_json::Value::is_array)
                })
        });
    replay
        .get("attestation_version")
        .and_then(|v| v.as_u64())
        == Some(4)
        && replay.get("checker").and_then(|v| v.as_str()) == Some("leanchecker")
        && replay.get("mode").and_then(|v| v.as_str()) == Some("ordinary")
        && replay.get("artifact_bundle") == Some(&serde_json::Value::Array(artifact_bundle))
        && replay
            .get("toolchain_sha256")
            .and_then(|v| v.as_str())
            == Some(toolchain_sha256.as_str())
        && replay
            .get("declaration_manifest_version")
            .and_then(|v| v.as_u64())
            == Some(2)
        && replay
            .get("trusted_import_policy_version")
            .and_then(|v| v.as_u64())
            == Some(1)
        && replay
            .get("trusted_direct_imports")
            .is_some_and(serde_json::Value::is_array)
        && declaration_manifest_is_valid
        && visibility_is_valid
}

/// Trusted cache consistency predicate shared with runtime-side observation
/// caches. Every Tablet module reachable from the requested nodes, plus the
/// shared Preamble, must carry a current source+replay record.
pub fn tablet_olean_closures_current(repo_path: &Path, nodes: &BTreeSet<NodeId>) -> bool {
    let mut closure: BTreeSet<String> = BTreeSet::new();
    for node in nodes {
        let cleaned = node.trim();
        if cleaned.is_empty() {
            continue;
        }
        crate::cache_key::recursive_imports(repo_path, cleaned, &mut closure);
        closure.insert(cleaned.to_string());
    }
    if closure.is_empty() {
        return true;
    }
    if repo_path.join("Tablet/Preamble.lean").is_file() {
        closure.insert("Preamble".to_string());
    }
    closure
        .iter()
        .all(|node| olean_content_current(repo_path, node))
}

/// Build the cache-lookup tuple from the canonicalised repo path and the
/// sorted, cleaned node names. The same node set yields the same tuple
/// independent of input ordering (BTreeSet sorts), and the canonical
/// path lets us coalesce symlink-different-but-content-equal views.
fn materialize_oleans_lookup_key(
    canon_repo: &Path,
    requested_nodes: &BTreeSet<NodeId>,
) -> (PathBuf, Vec<String>) {
    let cleaned_nodes: Vec<String> = requested_nodes
        .iter()
        .filter_map(|node| {
            let cleaned = node.trim();
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned.to_string())
            }
        })
        .collect();
    (canon_repo.to_path_buf(), cleaned_nodes)
}

fn cleaned_node_set(requested_nodes: &BTreeSet<NodeId>) -> BTreeSet<String> {
    requested_nodes
        .iter()
        .filter_map(|node| {
            let cleaned = node.trim();
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned.to_string())
            }
        })
        .collect()
}

fn materialize_tablet_oleans(
    repo_path: &Path,
    requested_nodes: &BTreeSet<NodeId>,
) -> Result<ExternalCommandObservation, String> {
    materialize_tablet_oleans_with_timeout(repo_path, requested_nodes, None)
}

fn materialize_tablet_oleans_with_timeout(
    repo_path: &Path,
    requested_nodes: &BTreeSet<NodeId>,
    timeout: Option<Duration>,
) -> Result<ExternalCommandObservation, String> {
    let canon_repo = fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    let lookup_key = materialize_oleans_lookup_key(&canon_repo, requested_nodes);

    // Cache key construction. `None` ⇒ cache skip (slow path runs).
    // Empty cleaned-node set ⇒ skip cache too: the materialize call
    // itself short-circuits trivially in the dispatch script when no
    // nodes are passed, so caching brings no benefit and the multi-node
    // key would compute the same value across all empty calls (which
    // is fine, but pointless).
    let cleaned_nodes = cleaned_node_set(requested_nodes);
    let cache_key = if cleaned_nodes.is_empty() {
        None
    } else {
        lean_closure_cache_key_for_nodes(repo_path, &cleaned_nodes)
    };

    // Two-tier cache:
    //   Tier 1 (in-memory): same kernel-binary process repeats. Cheap.
    //   Tier 2 (disk): cross-process / cross-cycle persistence. The
    //                  kernel binary is short-lived (Popen'd per
    //                  RuntimeCliRequest), so the in-memory tier alone
    //                  cannot persist across cycles — disk is what
    //                  closes the ~17-min materialize-oleans gap.
    //
    // Both tiers use the same source+replay consistency guard. A cleanup,
    // replacement, toolchain change, or unattested legacy olean therefore
    // forces the live materialize path even when the source key matches.
    if let Some(ref k) = cache_key {
        let cache = MATERIALIZE_OLEANS_CACHE.lock().unwrap();
        if let Some((stored_key, stored_value)) = cache.get(&lookup_key) {
            if stored_key == k && tablet_olean_closures_current(repo_path, requested_nodes) {
                // All inputs identical AND every expected olean still
                // on disk: serve the prior successful observation.
                return Ok(stored_value.clone());
            }
        }
    }

    // Tier 2: disk cache lookup. Walks the writable cache plus the
    // optional readonly fallback (set in worker contexts to point at
    // the supervisor's cache). Writes go to writable only — see the
    // `cache_dir_for_namespace` call further down.
    let disk_lookup_string = materialize_oleans_disk_lookup_key(&canon_repo, &lookup_key.1);
    if let Some(ref k) = cache_key {
        let dirs = cache_lookup_dirs(MATERIALIZE_OLEANS_DISK_NAMESPACE);
        if let Some(stored_value) =
            disk_cache_get_first::<ExternalCommandObservation>(&dirs, &disk_lookup_string, k)
        {
            if tablet_olean_closures_current(repo_path, requested_nodes) {
                // Promote the disk hit into Tier 1 so subsequent
                // calls in this process skip the disk read.
                MATERIALIZE_OLEANS_CACHE
                    .lock()
                    .unwrap()
                    .insert(lookup_key.clone(), (k.clone(), stored_value.clone()));
                return Ok(stored_value);
            }
        }
    }

    let driver = crate::backend::checker_driver_for(tablet_target_for_repo(repo_path));
    // Lean: `[repo, (--node N)*]` — per-node olean materialization (byte-
    // identical to the historical hardcoded-Lean construction). Isabelle:
    // `isabelle-build-session` builds the whole per-tablet session heap and
    // takes no `--node` flags (its CLI parser is optional positional
    // `node`/`repo`), so the kernel sends ONLY `[repo]`.
    let mut args = match driver {
        crate::backend::CheckerDriver::Lean(_) => {
            let mut args = vec![repo_path.display().to_string()];
            for node in requested_nodes {
                let cleaned = node.trim();
                if !cleaned.is_empty() {
                    args.push("--node".to_string());
                    args.push(cleaned.to_string());
                }
            }
            args
        }
        crate::backend::CheckerDriver::IsabelleHol(_) => {
            vec![repo_path.display().to_string()]
        }
    };
    if let Some(timeout) = timeout {
        args.push("--timeout-secs".to_string());
        args.push(format!("{:.3}", timeout.as_secs_f64().max(1.0)));
    }
    let raw = run_repo_command_json(repo_path, driver.materialize_oleans_op(), &args)?;
    let observation: ExternalCommandObservation = serde_json::from_value(raw)
        .map_err(|err| format!("parse {} output failed: {err}", driver.materialize_oleans_op()))?;

    // Only memoise unambiguous successes. A non-zero returncode is a
    // legitimate build failure that may resolve on the next call once
    // the worker fixes the underlying issue; pinning it would suppress
    // the recovery path. `timed_out` and `spawn_error` are also
    // exclusion criteria — transient infra failures must not pin.
    if observation.returncode == Some(0)
        && !observation.timed_out
        && observation.spawn_error.is_empty()
    {
        if let Some(k) = cache_key {
            MATERIALIZE_OLEANS_CACHE
                .lock()
                .unwrap()
                .insert(lookup_key, (k.clone(), observation.clone()));
            // Disk cache write is fire-and-forget; failures are silent
            // and the slow path always runs unchanged on next miss.
            if let Some(disk_dir) = cache_dir_for_namespace(MATERIALIZE_OLEANS_DISK_NAMESPACE) {
                disk_cache_put(&disk_dir, &disk_lookup_string, &k, &observation);
            }
        }
    }
    Ok(observation)
}

/// Test-only: drop this repo's memoised materialize-tablet-oleans
/// observations. Tests that mutate filesystem state in a single process
/// need this because the static cache otherwise persists across test
/// cases run by `cargo test`'s default parallel runner.
///
/// Scoped to `repo_path` rather than clearing the whole map: the cache
/// is process-global, so a blanket `.clear()` also evicted the entries
/// a *sibling* test had just populated, turning that test's expected
/// warm hit into a dispatch. Mirrors the repo-scoped
/// `clear_lean_semantic_payload_cache_for_tests`.
#[cfg(test)]
fn clear_materialize_oleans_cache_for_tests(repo_path: &Path) {
    let canon_repo = fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    MATERIALIZE_OLEANS_CACHE
        .lock()
        .unwrap()
        .retain(|(repo, _), _| repo != &canon_repo);
}

fn prepare_compiled_support(repo_path: &Path) -> Result<ExternalCommandObservation, String> {
    // `[repo]` arg shape is identical for both backends (Lean
    // `prepare-compiled-support` / Isabelle `isabelle-build-session`), so only
    // the driver selection is target-routed. Byte-identical for Lean.
    let driver = crate::backend::checker_driver_for(tablet_target_for_repo(repo_path));
    let raw = run_repo_command_json(
        repo_path,
        driver.prepare_compiled_support_op(),
        &[repo_path.display().to_string()],
    )?;
    serde_json::from_value(raw).map_err(|err| {
        format!(
            "parse {} output failed: {err}",
            driver.prepare_compiled_support_op()
        )
    })
}

fn ensure_external_command_ok(
    subcommand: &str,
    observation: &ExternalCommandObservation,
) -> Result<(), String> {
    if observation.timed_out {
        return Err(format!(
            "{subcommand} timed out{}",
            if observation.stderr.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", observation.stderr.trim())
            }
        ));
    }
    if !observation.spawn_error.trim().is_empty() {
        return Err(format!(
            "{subcommand} failed to start: {}",
            observation.spawn_error.trim()
        ));
    }
    if observation.returncode != Some(0) {
        let mut details = Vec::new();
        if !observation.stdout.trim().is_empty() {
            details.push(format!("stdout={:?}", observation.stdout.trim()));
        }
        if !observation.stderr.trim().is_empty() {
            details.push(format!("stderr={:?}", observation.stderr.trim()));
        }
        let detail_text = if details.is_empty() {
            String::new()
        } else {
            format!("; {}", details.join("; "))
        };
        return Err(format!(
            "{subcommand} failed with exit code {:?}{}",
            observation.returncode, detail_text
        ));
    }
    Ok(())
}


pub fn ensure_tablet_support_available(
    repo_path: &Path,
    requested_nodes: &BTreeSet<NodeId>,
) -> Result<TabletSupportSyncOutput, String> {
    let supervisor = sync_supervisor_workspace_from_repo(repo_path)?;
    let authoritative_repo = PathBuf::from(supervisor.authoritative_repo_path);
    let sync_output = sync_tablet_support_from_repo(&authoritative_repo)?;
    let prepared = prepare_compiled_support(&authoritative_repo)?;
    ensure_external_command_ok("prepare-compiled-support", &prepared)?;
    let materialization_nodes: BTreeSet<NodeId> = if requested_nodes.is_empty() {
        sync_output.root.node_names.iter().cloned().collect()
    } else {
        requested_nodes.clone()
    };
    let materialized = materialize_tablet_oleans(&authoritative_repo, &materialization_nodes)?;
    ensure_external_command_ok("materialize-tablet-oleans", &materialized)?;
    Ok(sync_output)
}

pub fn ensure_worker_checker_support_available(
    repo_path: &Path,
    requested_nodes: &BTreeSet<NodeId>,
) -> Result<TabletSupportObservation, String> {
    ensure_worker_checker_support_available_inner(repo_path, requested_nodes, None)
}

/// Budget-aware form used by long migration passes. The timeout is forwarded
/// to the checker-side materialization RPC so one cold owner cannot consume
/// the rest of the pass under the checker's ordinary one-hour default.
pub fn ensure_worker_checker_support_available_with_timeout(
    repo_path: &Path,
    requested_nodes: &BTreeSet<NodeId>,
    timeout: Duration,
) -> Result<TabletSupportObservation, String> {
    ensure_worker_checker_support_available_inner(repo_path, requested_nodes, Some(timeout))
}

fn ensure_worker_checker_support_available_inner(
    repo_path: &Path,
    requested_nodes: &BTreeSet<NodeId>,
    timeout: Option<Duration>,
) -> Result<TabletSupportObservation, String> {
    let support = sync_tablet_render_support_from_repo(repo_path)?;
    let materialization_nodes: BTreeSet<NodeId> = if requested_nodes.is_empty() {
        present_nodes_from_repo(repo_path)?
    } else {
        requested_nodes.clone()
    };
    let materialized =
        materialize_tablet_oleans_with_timeout(repo_path, &materialization_nodes, timeout)?;
    ensure_external_command_ok("materialize-tablet-oleans", &materialized)?;
    Ok(support)
}

/// Materialize oleans for the requested nodes WITHOUT re-running the
/// tablet-support render. Callers that have already invoked
/// `sync_tablet_render_support_from_repo` for the current repo state (e.g.
/// once at the start of a parallel observation batch) can use this to
/// materialize the per-node oleans they need without re-issuing the racy
/// `sync-tablet-support` subprocess. The render output is a pure function of
/// the repo tree, so a single upfront sync covers any number of subsequent
/// per-node materializations.
pub fn ensure_worker_checker_oleans_materialized(
    repo_path: &Path,
    requested_nodes: &BTreeSet<NodeId>,
) -> Result<(), String> {
    let materialization_nodes: BTreeSet<NodeId> = if requested_nodes.is_empty() {
        present_nodes_from_repo(repo_path)?
    } else {
        requested_nodes.clone()
    };
    let materialized = materialize_tablet_oleans(repo_path, &materialization_nodes)?;
    ensure_external_command_ok("materialize-tablet-oleans", &materialized)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir_in;

    #[test]
    fn regenerated_support_filename_set_excludes_persistent_structural_files() {
        assert!(is_kernel_regenerated_tablet_support_filename("INDEX.md"));
        assert!(is_kernel_regenerated_tablet_support_filename("README.md"));
        assert!(!is_kernel_regenerated_tablet_support_filename("header.tex"));
        assert!(!is_kernel_regenerated_tablet_support_filename("Preamble.tex"));
    }

    #[test]
    fn build_tablet_support_snapshot_tracks_repo_state_without_tablet_json() {
        let tmp_root = std::env::current_dir()
            .expect("current dir")
            .join(".tmp-tests");
        fs::create_dir_all(&tmp_root).expect("tmp root");
        let tmp = tempdir_in(&tmp_root).expect("tempdir");
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).expect("tablet dir");
        fs::write(
            repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        )
        .expect("write preamble lean");
        fs::write(
            repo.join("Tablet/Preamble.tex"),
            "\\begin{definition}[Ambient setup]\\label{pre:a}Setup\\end{definition}\n",
        )
        .expect("write preamble tex");
        fs::write(
            repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ndef A : Nat := by\n  sorry\n",
        )
        .expect("write A lean");
        fs::write(
            repo.join("Tablet/A.tex"),
            "\\begin{definition}[Alpha]\\label{def:alpha}A\\end{definition}\n",
        )
        .expect("write A tex");

        let snapshot =
            build_tablet_support_snapshot_from_repo(&repo).expect("build support snapshot");

        assert_eq!(snapshot.metrics.total_nodes, 1);
        assert_eq!(snapshot.metrics.open_nodes, 1);
        assert_eq!(snapshot.metrics.closed_nodes, 0);
        assert_eq!(
            snapshot.nodes,
            vec![
                TabletSupportNodeSnapshot {
                    name: NodeId::from("A"),
                    env: "definition".to_string(),
                    kind: "definition".to_string(),
                    status: "open".to_string(),
                    title: "Alpha".to_string(),
                    refs: vec!["def:alpha".to_string()],
                    imports: vec![NodeId::from("Preamble")],
                },
                TabletSupportNodeSnapshot {
                    name: NodeId::from("Preamble"),
                    env: "preamble".to_string(),
                    kind: "preamble".to_string(),
                    status: "closed".to_string(),
                    title: String::new(),
                    refs: Vec::new(),
                    imports: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn build_tablet_support_render_output_moves_rendering_policy_into_rust() {
        let tmp_root = std::env::current_dir()
            .expect("current dir")
            .join(".tmp-tests");
        fs::create_dir_all(&tmp_root).expect("tmp root");
        let tmp = tempdir_in(&tmp_root).expect("tempdir");
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).expect("tablet dir");
        let snapshot = TabletSupportSnapshot {
            nodes: vec![
                TabletSupportNodeSnapshot {
                    name: NodeId::from("A"),
                    env: "definition".to_string(),
                    kind: "definition".to_string(),
                    status: "open".to_string(),
                    title: "Alpha".to_string(),
                    refs: vec!["def:alpha".to_string()],
                    imports: vec![NodeId::from("Preamble")],
                },
                TabletSupportNodeSnapshot {
                    name: NodeId::from("Preamble"),
                    env: "preamble".to_string(),
                    kind: "preamble".to_string(),
                    status: "closed".to_string(),
                    title: String::new(),
                    refs: Vec::new(),
                    imports: Vec::new(),
                },
            ],
            metrics: TabletSupportMetricsSnapshot {
                total_nodes: 1,
                closed_nodes: 0,
                open_nodes: 1,
            },
        };

        let render = build_tablet_support_render_output(&repo, &snapshot);

        assert_eq!(
            render.index_md_path,
            repo.join("Tablet/INDEX.md").display().to_string()
        );
        assert!(render
            .index_md_content
            .contains("| A | definition | definition | open |"));
        assert_eq!(
            render.readme_md_path,
            repo.join("Tablet/README.md").display().to_string()
        );
        assert!(render.readme_md_content.contains("## Nodes With Labels"));
        assert_eq!(render.header_tex_content, Some(generate_header_tex()));

        fs::write(repo.join("Tablet/header.tex"), "% keep\n").expect("write header");
        let second = build_tablet_support_render_output(&repo, &snapshot);
        assert_eq!(second.header_tex_content, None);
    }

    // ------ materialize_tablet_oleans short-circuit cache -----------------
    //
    // These tests exercise the process-local content-hash cache that
    // gates `materialize_tablet_oleans` (the single most expensive op in
    // a fingerprint walk — ~17min per call on a typical 90-node closure).
    //
    // The pattern: a counting stub `check.py` records each subcommand
    // invocation to a log file. We call `materialize_tablet_oleans`
    // through public-facing wrappers and inter-call edits, then count
    // the recorded invocations to confirm hit/miss behaviour.

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn write_test_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn write_node_olean(repo: &Path, node: &str) {
        let targets: Vec<&str> = if node == "Preamble" {
            vec![node]
        } else {
            vec!["Preamble", node]
        };
        for target in targets {
            let path = repo
                .join(".lake/build/lib/lean/Tablet")
                .join(format!("{target}.olean"));
            write_test_file(&path, &format!("olean-stub-{target}"));
            // The real checker records unified source + exact-byte replay
            // provenance for the entire closure, including Preamble.
            let Some(key) = crate::cache_key::lean_closure_cache_key(repo, target) else {
                continue;
            };
            let sidecar = repo
                .join(".lake/build/lib/lean/Tablet")
                .join(format!("{target}.olean.srcclosure"));
            let server_path = PathBuf::from(format!("{}.server", path.display()));
            let private_path = PathBuf::from(format!("{}.private", path.display()));
            let mut part_paths = vec![("exported", path.clone())];
            if server_path.exists() {
                part_paths.push(("server", server_path));
                if private_path.exists() {
                    part_paths.push(("private", private_path));
                }
            }
            let artifact_bundle: Vec<_> = part_paths
                .iter()
                .map(|(level, part_path)| {
                    let bytes = fs::read(part_path).expect("read test artifact part");
                    serde_json::json!({
                        "level": level,
                        "sha256": crate::cache_key::hash_bytes(&bytes),
                        "size_bytes": bytes.len(),
                    })
                })
                .collect();
            let declaration_manifest = if target == "Preamble" {
                Vec::new()
            } else {
                vec![serde_json::json!({"name": target, "kind": "theorem"})]
            };
            let visibility_manifests: Vec<_> = artifact_bundle
                .iter()
                .map(|part| {
                    serde_json::json!({
                        "level": part.get("level").expect("part level"),
                        "declarations": declaration_manifest,
                    })
                })
                .collect();
            let toolchain_sha256 = crate::cache_key::hash_bytes(
                &fs::read(repo.join("lean-toolchain")).unwrap_or_default(),
            );
            let record = serde_json::json!({
                "schema_version": 3,
                "source_closure_sha256": key,
                "kernel_replay": {
                    "attestation_version": 4,
                    "checker": "leanchecker",
                    "mode": "ordinary",
                    "artifact_bundle": artifact_bundle,
                    "toolchain_sha256": toolchain_sha256,
                    "declaration_manifest_version": 2,
                    "declaration_manifest": declaration_manifest,
                    "visibility_manifests": visibility_manifests,
                    "trusted_import_policy_version": 1,
                    "trusted_direct_imports": [],
                }
            });
            write_test_file(&sidecar, &format!("{record}\n"));
        }
    }

    fn count_invocations(log_path: &Path, subcommand: &str) -> usize {
        let raw = fs::read_to_string(log_path).unwrap_or_default();
        raw.lines().filter(|line| line.trim() == subcommand).count()
    }

    fn install_counting_stub_check_script(repo: &Path, log_path: &Path) {
        // The stub records each invocation, then emits a JSON success
        // payload shaped like `ExternalCommandObservation`. Used
        // unconditionally for `materialize-tablet-oleans` so the cache
        // hit-vs-miss assertion can count invocations.
        let script = format!(
            r#"#!/usr/bin/env python3
import json
import sys
from pathlib import Path

cmd = sys.argv[1]
with Path({log_path:?}).open("a", encoding="utf-8") as h:
    h.write(cmd + "\n")

if cmd == "materialize-tablet-oleans":
    print(json.dumps({{
        "returncode": 0,
        "stdout": "materialized",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }}))
else:
    raise SystemExit(f"unexpected subcommand: {{cmd}}")
"#,
            log_path = log_path.display().to_string(),
        );
        let path = repo.join(".trellis/scripts/check.py");
        write_test_file(&path, &script);
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).unwrap();
        }
    }

    fn install_failing_then_succeeding_stub(repo: &Path, log_path: &Path, toggle: &Path) {
        // First materialize-tablet-oleans call returns failure; second
        // (after toggle file appears) returns success. Used to verify
        // failed observations are not memoised.
        let script = format!(
            r#"#!/usr/bin/env python3
import json
import sys
from pathlib import Path

cmd = sys.argv[1]
with Path({log_path:?}).open("a", encoding="utf-8") as h:
    h.write(cmd + "\n")

toggle = Path({toggle:?})
if cmd == "materialize-tablet-oleans":
    if not toggle.exists():
        toggle.write_text("on")
        print(json.dumps({{
            "returncode": 1,
            "stdout": "",
            "stderr": "lake build failed",
            "timed_out": False,
            "spawn_error": "",
        }}))
    else:
        print(json.dumps({{
            "returncode": 0,
            "stdout": "materialized",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }}))
else:
    raise SystemExit(f"unexpected: {{cmd}}")
"#,
            log_path = log_path.display().to_string(),
            toggle = toggle.display().to_string(),
        );
        let path = repo.join(".trellis/scripts/check.py");
        write_test_file(&path, &script);
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).unwrap();
        }
    }

    fn seed_minimal_lake_repo(repo: &Path) {
        write_test_file(&repo.join("lakefile.lean"), "package «stub»\n");
        write_test_file(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
    }

    #[test]
    fn materialize_oleans_cache_skips_dispatch_on_unchanged_inputs_with_oleans_present() {
        let tmp_root = std::env::current_dir().unwrap().join(".tmp-tests");
        fs::create_dir_all(&tmp_root).unwrap();
        let tmp = tempdir_in(&tmp_root).unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        seed_minimal_lake_repo(&repo);
        install_counting_stub_check_script(&repo, &log);
        write_test_file(
            &repo.join("Tablet/UnchangedInputs.lean"),
            "import Tablet.Preamble\n\ntheorem UnchangedInputs : True := trivial\n",
        );
        write_node_olean(&repo, "UnchangedInputs");

        clear_materialize_oleans_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from("UnchangedInputs")].into_iter().collect();

        // Cold cache: first call must dispatch.
        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(
            count_invocations(&log, "materialize-tablet-oleans"),
            1,
            "cold cache must dispatch materialize-tablet-oleans"
        );

        // Warm cache + olean still on disk: must skip the dispatch.
        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(
            count_invocations(&log, "materialize-tablet-oleans"),
            1,
            "warm cache hit with olean present must skip dispatch"
        );
    }

    #[test]
    fn materialize_oleans_cache_clear_is_scoped_to_each_test_repo() {
        // Deterministic reproduction of the parallel-suite flake: one sibling
        // test clearing its cache setup must not evict a different test's
        // warm repo entry between that test's two calls.
        let tmp_root = std::env::current_dir().unwrap().join(".tmp-tests");
        fs::create_dir_all(&tmp_root).unwrap();
        let tmp = tempdir_in(&tmp_root).unwrap();
        let repo_a = tmp.path().join("repo-a");
        let repo_b = tmp.path().join("repo-b");
        let log_a = tmp.path().join("invocations-a.log");
        let log_b = tmp.path().join("invocations-b.log");
        for (repo, log) in [(&repo_a, &log_a), (&repo_b, &log_b)] {
            seed_minimal_lake_repo(repo);
            install_counting_stub_check_script(repo, log);
            write_test_file(
                &repo.join("Tablet/A.lean"),
                "import Tablet.Preamble\n\ntheorem A : True := trivial\n",
            );
            write_node_olean(repo, "A");
        }
        let nodes: BTreeSet<NodeId> = [NodeId::from("A")].into_iter().collect();

        clear_materialize_oleans_cache_for_tests(&repo_a);
        clear_materialize_oleans_cache_for_tests(&repo_b);
        materialize_tablet_oleans(&repo_a, &nodes).unwrap();
        materialize_tablet_oleans(&repo_b, &nodes).unwrap();

        // Simulate repo B's sibling test beginning after repo A warmed.
        clear_materialize_oleans_cache_for_tests(&repo_b);
        materialize_tablet_oleans(&repo_a, &nodes).unwrap();
        assert_eq!(
            count_invocations(&log_a, "materialize-tablet-oleans"),
            1,
            "clearing one test repo must not evict another test's warm cache entry"
        );
    }

    #[test]
    fn materialize_oleans_cache_invalidates_when_input_lean_changes() {
        let tmp_root = std::env::current_dir().unwrap().join(".tmp-tests");
        fs::create_dir_all(&tmp_root).unwrap();
        let tmp = tempdir_in(&tmp_root).unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        seed_minimal_lake_repo(&repo);
        install_counting_stub_check_script(&repo, &log);
        write_test_file(
            &repo.join("Tablet/InputLeanChanges.lean"),
            "import Tablet.Preamble\n\ntheorem InputLeanChanges : True := trivial\n",
        );
        write_node_olean(&repo, "InputLeanChanges");

        clear_materialize_oleans_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from("InputLeanChanges")].into_iter().collect();

        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(count_invocations(&log, "materialize-tablet-oleans"), 1);

        // Worker edits the .lean — content hash shifts ⇒ cache miss.
        write_test_file(
            &repo.join("Tablet/InputLeanChanges.lean"),
            "import Tablet.Preamble\n\ntheorem InputLeanChanges : True := by trivial\n",
        );

        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(
            count_invocations(&log, "materialize-tablet-oleans"),
            2,
            ".lean edit must force a fresh materialize-tablet-oleans dispatch"
        );
    }

    #[test]
    fn materialize_oleans_cache_falls_back_when_olean_missing_despite_key_match() {
        // Conservative-by-design: even when the closure key matches,
        // a missing olean must trigger a fresh dispatch so the artefact
        // gets rebuilt. Otherwise downstream lean ops break.
        let tmp_root = std::env::current_dir().unwrap().join(".tmp-tests");
        fs::create_dir_all(&tmp_root).unwrap();
        let tmp = tempdir_in(&tmp_root).unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        seed_minimal_lake_repo(&repo);
        install_counting_stub_check_script(&repo, &log);
        write_test_file(
            &repo.join("Tablet/OleanMissingKeyMatch.lean"),
            "import Tablet.Preamble\n\ntheorem OleanMissingKeyMatch : True := trivial\n",
        );
        write_node_olean(&repo, "OleanMissingKeyMatch");

        clear_materialize_oleans_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from("OleanMissingKeyMatch")].into_iter().collect();

        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(count_invocations(&log, "materialize-tablet-oleans"), 1);

        // Worker hygiene cleanup wipes the olean while .lean stays put.
        fs::remove_file(repo.join(".lake/build/lib/lean/Tablet/OleanMissingKeyMatch.olean")).unwrap();

        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(
            count_invocations(&log, "materialize-tablet-oleans"),
            2,
            "missing olean must force a fresh materialize-tablet-oleans dispatch"
        );
    }

    #[test]
    fn materialize_oleans_cache_falls_back_when_olean_content_stale() {
        // Olean-staleness fix (end-to-end): even when the source-closure
        // cache KEY matches (source content unchanged), a present olean whose
        // provenance sidecar does NOT match the current key — the
        // rewind-round-trip / operator-touch shape where the on-disk olean
        // was built from a different intermediate state — must NOT be served
        // from cache. The content guard forces a fresh dispatch.
        let tmp_root = std::env::current_dir().unwrap().join(".tmp-tests");
        fs::create_dir_all(&tmp_root).unwrap();
        let tmp = tempdir_in(&tmp_root).unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        seed_minimal_lake_repo(&repo);
        install_counting_stub_check_script(&repo, &log);
        write_test_file(
            &repo.join("Tablet/OleanContentStale.lean"),
            "import Tablet.Preamble\n\ntheorem OleanContentStale : True := trivial\n",
        );
        write_node_olean(&repo, "OleanContentStale");

        clear_materialize_oleans_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from("OleanContentStale")].into_iter().collect();

        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(count_invocations(&log, "materialize-tablet-oleans"), 1);

        // Stomp the provenance sidecar with a stale hash (olean now
        // content-stale even though the source — and thus the cache key — is
        // unchanged). The cache-hit guard must reject it.
        let sidecar = repo.join(".lake/build/lib/lean/Tablet/OleanContentStale.olean.srcclosure");
        write_test_file(&sidecar, "0000000000000000000000000000000000000000000000000000000000000000\n");

        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(
            count_invocations(&log, "materialize-tablet-oleans"),
            2,
            "content-stale olean (sidecar mismatch) must force a fresh dispatch despite key match",
        );
    }

    #[test]
    fn materialize_oleans_cache_falls_back_when_replay_attestation_is_stale() {
        let tmp_root = std::env::current_dir().unwrap().join(".tmp-tests");
        fs::create_dir_all(&tmp_root).unwrap();
        let tmp = tempdir_in(&tmp_root).unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        let node = "ReplayAttestationStale";
        seed_minimal_lake_repo(&repo);
        install_counting_stub_check_script(&repo, &log);
        write_test_file(
            &repo.join(format!("Tablet/{node}.lean")),
            &format!("import Tablet.Preamble\n\ntheorem {node} : True := trivial\n"),
        );
        write_node_olean(&repo, node);

        clear_materialize_oleans_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from(node)].into_iter().collect();
        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(count_invocations(&log, "materialize-tablet-oleans"), 1);

        // The source provenance stays current, but these are no longer the
        // exact bytes leanchecker attested. The source-only cache key still
        // matches, so only the replay consistency guard can force the miss.
        write_test_file(
            &repo.join(format!(".lake/build/lib/lean/Tablet/{node}.olean")),
            "post-attestation replacement",
        );

        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(
            count_invocations(&log, "materialize-tablet-oleans"),
            2,
            "an exact-byte replay mismatch must force a live materialize dispatch",
        );
    }

    #[test]
    fn materialize_oleans_cache_binds_server_and_private_artifact_parts() {
        let tmp_root = std::env::current_dir().unwrap().join(".tmp-tests");
        fs::create_dir_all(&tmp_root).unwrap();
        let tmp = tempdir_in(&tmp_root).unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        let node = "SplitReplayAttestation";
        seed_minimal_lake_repo(&repo);
        install_counting_stub_check_script(&repo, &log);
        write_test_file(
            &repo.join(format!("Tablet/{node}.lean")),
            &format!("import Tablet.Preamble\n\ntheorem {node} : True := trivial\n"),
        );
        write_node_olean(&repo, node);
        let exported = repo.join(format!(".lake/build/lib/lean/Tablet/{node}.olean"));
        write_test_file(
            &PathBuf::from(format!("{}.server", exported.display())),
            "server-part-v1",
        );
        let private = PathBuf::from(format!("{}.private", exported.display()));
        write_test_file(&private, "private-part-v1");
        // Re-emit the test provenance now that the ordered split bundle exists.
        write_node_olean(&repo, node);

        clear_materialize_oleans_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from(node)].into_iter().collect();
        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(count_invocations(&log, "materialize-tablet-oleans"), 1);

        write_test_file(&private, "private-part-v2");
        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(
            count_invocations(&log, "materialize-tablet-oleans"),
            2,
            "a private-only byte change must invalidate the trusted materialization cache",
        );
    }

    #[test]
    fn materialize_oleans_cache_does_not_pin_failed_returncode() {
        // A non-zero returncode is a legitimate build failure that may
        // resolve on the next call (worker fixes the source, lake-lock
        // contention clears, etc.). Caching the failure would lock out
        // the recovery path; we must dispatch again on the next call.
        let tmp_root = std::env::current_dir().unwrap().join(".tmp-tests");
        fs::create_dir_all(&tmp_root).unwrap();
        let tmp = tempdir_in(&tmp_root).unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        let toggle = tmp.path().join("toggle");
        seed_minimal_lake_repo(&repo);
        install_failing_then_succeeding_stub(&repo, &log, &toggle);
        write_test_file(
            &repo.join("Tablet/FailedReturncode.lean"),
            "import Tablet.Preamble\n\ntheorem FailedReturncode : True := trivial\n",
        );
        write_node_olean(&repo, "FailedReturncode");

        clear_materialize_oleans_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from("FailedReturncode")].into_iter().collect();

        let first = materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(first.returncode, Some(1));
        assert_eq!(count_invocations(&log, "materialize-tablet-oleans"), 1);

        // Failed observation must not be pinned ⇒ second call dispatches.
        let second = materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(second.returncode, Some(0));
        assert_eq!(
            count_invocations(&log, "materialize-tablet-oleans"),
            2,
            "failed materialize must not be cached"
        );
    }

    #[test]
    fn materialize_oleans_empty_node_set_short_circuits_in_dispatch_layer() {
        // Empty node set: the cache layer skips memoisation (the multi-
        // node key would be a no-op anyway), and the call passes through
        // to the underlying script unconditionally. The script's
        // own short-circuit (no `--node` args ⇒ trivial empty walk)
        // applies; the kernel-side observation still counts as a live
        // dispatch so caller ergonomics are preserved.
        let tmp_root = std::env::current_dir().unwrap().join(".tmp-tests");
        fs::create_dir_all(&tmp_root).unwrap();
        let tmp = tempdir_in(&tmp_root).unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        seed_minimal_lake_repo(&repo);
        install_counting_stub_check_script(&repo, &log);

        clear_materialize_oleans_cache_for_tests(&repo);
        let empty: BTreeSet<NodeId> = BTreeSet::new();

        // Each call dispatches because there's no cache key for an
        // empty node set — but every call still completes successfully
        // (the underlying script is a no-op for empty input).
        let first = materialize_tablet_oleans(&repo, &empty).unwrap();
        assert_eq!(first.returncode, Some(0));
        assert_eq!(count_invocations(&log, "materialize-tablet-oleans"), 1);

        let second = materialize_tablet_oleans(&repo, &empty).unwrap();
        assert_eq!(second.returncode, Some(0));
        assert_eq!(
            count_invocations(&log, "materialize-tablet-oleans"),
            2,
            "empty-node-set call passes through unconditionally (no key to cache)"
        );
    }

    #[test]
    fn materialize_oleans_cache_invalidates_when_lake_state_changes() {
        // A `lake-manifest.json` change (e.g. mathlib version bump) must
        // invalidate the cache for every node — otherwise stale lake
        // state would silently pin to outdated build outputs.
        let tmp_root = std::env::current_dir().unwrap().join(".tmp-tests");
        fs::create_dir_all(&tmp_root).unwrap();
        let tmp = tempdir_in(&tmp_root).unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        seed_minimal_lake_repo(&repo);
        install_counting_stub_check_script(&repo, &log);
        write_test_file(
            &repo.join("Tablet/LakeStateChanges.lean"),
            "import Tablet.Preamble\n\ntheorem LakeStateChanges : True := trivial\n",
        );
        write_test_file(&repo.join("lake-manifest.json"), "{\"version\":1}\n");
        write_node_olean(&repo, "LakeStateChanges");

        clear_materialize_oleans_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from("LakeStateChanges")].into_iter().collect();

        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(count_invocations(&log, "materialize-tablet-oleans"), 1);

        // Mathlib version bump.
        write_test_file(&repo.join("lake-manifest.json"), "{\"version\":2}\n");

        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(
            count_invocations(&log, "materialize-tablet-oleans"),
            2,
            "lake-manifest change must invalidate the materialize-oleans cache"
        );
    }

    /// Point the disk cache at `cache_root` for the duration of `body`.
    ///
    /// This used to hold a module-private mutex of its own, which left
    /// the crate with three uncoordinated disciplines over the same
    /// process-global env var. A `setenv` here can tear a concurrent
    /// `getenv` of an unrelated variable, so the mutation must be
    /// serialized against every env-sensitive test in the binary, not
    /// just against the other disk-cache tests. `EnvScope` takes the one
    /// crate-wide lock and restores on drop, panic included.
    fn with_disk_cache_root<R>(cache_root: &Path, body: impl FnOnce() -> R) -> R {
        let _env = crate::EnvScope::with_kernel_cache_root(cache_root);
        body()
    }

    #[test]
    fn materialize_oleans_disk_cache_skips_dispatch_after_in_memory_cleared() {
        // Simulates the production process shape: kernel CLI process N
        // populates the disk cache. Process N+1 starts cold (in-memory
        // empty) but reads the disk cache and short-circuits without
        // dispatching `materialize-tablet-oleans` again.
        let tmp_root = std::env::current_dir().unwrap().join(".tmp-tests");
        fs::create_dir_all(&tmp_root).unwrap();
        let tmp = tempdir_in(&tmp_root).unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        let cache_root = tmp.path().join("runtime-root");
        fs::create_dir_all(&cache_root).unwrap();
        seed_minimal_lake_repo(&repo);
        install_counting_stub_check_script(&repo, &log);
        write_test_file(
            &repo.join("Tablet/DiskWarmHit.lean"),
            "import Tablet.Preamble\n\ntheorem DiskWarmHit : True := trivial\n",
        );
        write_node_olean(&repo, "DiskWarmHit");

        let nodes: BTreeSet<NodeId> = [NodeId::from("DiskWarmHit")].into_iter().collect();

        with_disk_cache_root(&cache_root, || {
            // "Process 1": cold, dispatches once, populates both tiers.
            clear_materialize_oleans_cache_for_tests(&repo);
            materialize_tablet_oleans(&repo, &nodes).unwrap();
            assert_eq!(
                count_invocations(&log, "materialize-tablet-oleans"),
                1,
                "cold cache must dispatch once"
            );

            // "Process 2": fresh in-memory cache, but disk persists.
            clear_materialize_oleans_cache_for_tests(&repo);
            materialize_tablet_oleans(&repo, &nodes).unwrap();
            assert_eq!(
                count_invocations(&log, "materialize-tablet-oleans"),
                1,
                "warm DISK cache (after cold in-memory) must skip dispatch"
            );
        });
    }

    #[test]
    fn materialize_oleans_disk_cache_falls_back_when_oleans_missing() {
        // Olean-presence guard must apply to disk-tier hits too: even
        // if the closure-content key matches the cached observation, a
        // missing olean forces a fresh dispatch so the artefact gets
        // rebuilt.
        let tmp_root = std::env::current_dir().unwrap().join(".tmp-tests");
        fs::create_dir_all(&tmp_root).unwrap();
        let tmp = tempdir_in(&tmp_root).unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        let cache_root = tmp.path().join("runtime-root");
        fs::create_dir_all(&cache_root).unwrap();
        seed_minimal_lake_repo(&repo);
        install_counting_stub_check_script(&repo, &log);
        write_test_file(
            &repo.join("Tablet/DiskOleansMissing.lean"),
            "import Tablet.Preamble\n\ntheorem DiskOleansMissing : True := trivial\n",
        );
        write_node_olean(&repo, "DiskOleansMissing");

        let nodes: BTreeSet<NodeId> = [NodeId::from("DiskOleansMissing")].into_iter().collect();

        with_disk_cache_root(&cache_root, || {
            clear_materialize_oleans_cache_for_tests(&repo);
            materialize_tablet_oleans(&repo, &nodes).unwrap();
            assert_eq!(count_invocations(&log, "materialize-tablet-oleans"), 1);

            // Worker hygiene cleanup wipes the olean.
            fs::remove_file(repo.join(".lake/build/lib/lean/Tablet/DiskOleansMissing.olean")).unwrap();
            // New "process": in-memory cleared. Disk cache key would
            // match, but olean-presence guard must force a fresh
            // dispatch.
            clear_materialize_oleans_cache_for_tests(&repo);
            materialize_tablet_oleans(&repo, &nodes).unwrap();
            assert_eq!(
                count_invocations(&log, "materialize-tablet-oleans"),
                2,
                "disk-cache hit must still verify olean presence"
            );
        });
    }

    // ------ Isabelle sync-session response parse (external-command envelope) --
    //
    // `sync_tablet_render_support_from_repo` branches its RESPONSE PARSE per
    // target. For Lean it parses the render-tracking `TabletSupportObservation`
    // (required `updated_paths`). For Isabelle the `_isabelle_sync_session`
    // handler returns the EXTERNAL-COMMAND ENVELOPE (`{returncode, stdout,
    // stderr, timed_out, spawn_error}`, the scaffold summary inside `stdout`,
    // NO top-level `updated_paths`) — the same shape every other Isabelle op
    // uses. These tests pin: (1) a `returncode: 0` envelope parses as SUCCESS
    // (it would error with `missing field updated_paths` under the Lean
    // parse — the original wire-shape defect); (2) a `returncode: null`
    // envelope FAILS CLOSED, mirroring how the other Isabelle ops consume the
    // envelope.

    /// Mark `repo` as an Isabelle-target repo (the resolver
    /// `tablet_target_for_repo` reads `trellis.config.json`'s
    /// `workflow.default_target`).
    fn mark_isabelle_target(repo: &Path) {
        write_test_file(
            &repo.join("trellis.config.json"),
            r#"{"workflow":{"default_target":"isabelle_hol"}}"#,
        );
    }

    /// Install a stub `check.py` whose `isabelle-sync-session` dispatch emits
    /// the external-command envelope shape (NO top-level `updated_paths`):
    /// `returncode: 0` + a JSON scaffold summary in `stdout` when
    /// `fail_closed` is false, else `returncode: null` + a `spawn_error`
    /// (the handler's write-failure fail-closed path).
    fn install_isabelle_sync_stub(repo: &Path, fail_closed: bool) {
        let body = if fail_closed {
            r#"print(json.dumps({
        "returncode": None,
        "stdout": "",
        "stderr": "isabelle session scaffold sync failed: boom",
        "timed_out": False,
        "spawn_error": "isabelle session scaffold sync failed: boom",
    }))"#
        } else {
            r#"print(json.dumps({
        "returncode": 0,
        "stdout": json.dumps({"updated": ["ROOT", "Tablet_Preamble.thy"]}),
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }))"#
        };
        let script = format!(
            r#"#!/usr/bin/env python3
import json
import sys

cmd = sys.argv[1]
if cmd == "isabelle-sync-session":
    {body}
else:
    raise SystemExit(f"unexpected subcommand: {{cmd}}")
"#,
            body = body,
        );
        let path = repo.join(".trellis/scripts/check.py");
        write_test_file(&path, &script);
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).unwrap();
        }
    }

    #[test]
    fn isabelle_sync_session_envelope_parses_as_success() {
        // The `_isabelle_sync_session` success envelope (returncode 0 + a
        // `stdout` summary, NO `updated_paths`) must be accepted. Under the
        // Lean `TabletSupportObservation` parse this would fail with
        // `missing field updated_paths` — the exact wire-shape defect this
        // fix addresses.
        let tmp_root = std::env::current_dir().unwrap().join(".tmp-tests");
        fs::create_dir_all(&tmp_root).unwrap();
        let tmp = tempdir_in(&tmp_root).unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        mark_isabelle_target(&repo);
        install_isabelle_sync_stub(&repo, false);

        let observation =
            sync_tablet_render_support_from_repo(&repo).expect("isabelle sync envelope is success");

        // The kernel does not track Isabelle's `updated_paths` (the server
        // owns the scaffold render); success synthesizes an empty list with
        // the canonical Tablet render paths.
        assert!(
            observation.updated_paths.is_empty(),
            "isabelle sync does not surface kernel-tracked updated_paths"
        );
        assert_eq!(
            observation.index_md_path,
            repo.join("Tablet/INDEX.md").display().to_string()
        );
    }

    #[test]
    fn isabelle_sync_session_returncode_null_fails_closed() {
        // A `returncode: null` envelope (the handler's write-failure fail-
        // closed path) must surface as an Err — the precondition treats it
        // as a hard failure, never a silent success. This preserves the
        // fail-closed-on-returncode-null contract the other Isabelle ops
        // already honour.
        let tmp_root = std::env::current_dir().unwrap().join(".tmp-tests");
        fs::create_dir_all(&tmp_root).unwrap();
        let tmp = tempdir_in(&tmp_root).unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        mark_isabelle_target(&repo);
        install_isabelle_sync_stub(&repo, true);

        let result = sync_tablet_render_support_from_repo(&repo);
        let err = result.expect_err("returncode:null envelope must fail closed");
        assert!(
            err.contains("isabelle-sync-session"),
            "fail-closed error should name the op; got: {err}"
        );
    }
}
