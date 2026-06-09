use crate::cache_key::lean_closure_cache_key_for_nodes;
use crate::disk_cache::{
    cache_dir_for_namespace, cache_lookup_dirs, disk_cache_get_first, disk_cache_put,
};
use crate::model::{NodeId, NodeKind};
use crate::tablet_root::{sync_tablet_root_from_repo, TabletRootSyncOutput};
use crate::worker_normalization::{
    direct_deps_from_repo, node_kinds_from_repo, open_nodes_from_repo, present_nodes_from_repo,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};

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
/// (carries `cache_v=2`).
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

fn node_tex_path(repo_path: &Path, node: &str) -> PathBuf {
    repo_path.join("Tablet").join(format!("{node}.tex"))
}

fn tablet_dir(repo_path: &Path) -> PathBuf {
    repo_path.join("Tablet")
}

fn index_md_path(repo_path: &Path) -> PathBuf {
    tablet_dir(repo_path).join("INDEX.md")
}

fn readme_md_path(repo_path: &Path) -> PathBuf {
    tablet_dir(repo_path).join("README.md")
}

fn header_tex_path(repo_path: &Path) -> PathBuf {
    tablet_dir(repo_path).join("header.tex")
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
    let support_snapshot = build_tablet_support_snapshot_from_repo(repo_path)?;
    let render_output = build_tablet_support_render_output(repo_path, &support_snapshot);
    let render_json = serde_json::to_string(&render_output)
        .map_err(|err| format!("serialize tablet support render failed: {err}"))?;
    // Pipe the render payload via stdin rather than argv: with ~420 tablet
    // nodes the rendered INDEX/README markdown easily exceeds 100 KB, which
    // pushed the spawn over Linux's ARG_MAX and broke acceptance checks.
    let raw = run_repo_command_json_with_stdin(
        repo_path,
        "sync-tablet-support",
        &[
            repo_path.display().to_string(),
            "--render-json".to_string(),
            "-".to_string(),
        ],
        Some(&render_json),
    )?;
    serde_json::from_value(raw)
        .map_err(|err| format!("parse sync-tablet-support output failed: {err}"))
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

/// Conservative on-disk verification before serving a cached
/// materialize-tablet-oleans success: every requested node's olean must
/// still be present. The cached result reports "lake produced oleans
/// for this closure", but if any have been deleted (worker hygiene
/// cleanup, GC, manual rm, `.lake` wipe), serving the cache hit would
/// silently leave a downstream lean op without its required artefact.
fn olean_present(repo_path: &Path, node: &str) -> bool {
    repo_path
        .join(".lake/build/lib/lean/Tablet")
        .join(format!("{node}.olean"))
        .exists()
}

fn all_oleans_present(repo_path: &Path, nodes: &BTreeSet<NodeId>) -> bool {
    nodes.iter().all(|node| {
        let cleaned = node.trim();
        if cleaned.is_empty() {
            true
        } else {
            olean_present(repo_path, cleaned)
        }
    })
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
    // Both tiers gated by the *same* olean-presence guard: even on key
    // match, every requested node's `Tablet/<node>.olean` must exist on
    // disk. A worker-hygiene cleanup, manual rm, or `.lake` wipe between
    // observations would otherwise leave the cached "build succeeded"
    // answer pinned while the artefact is gone.
    if let Some(ref k) = cache_key {
        let cache = MATERIALIZE_OLEANS_CACHE.lock().unwrap();
        if let Some((stored_key, stored_value)) = cache.get(&lookup_key) {
            if stored_key == k && all_oleans_present(repo_path, requested_nodes) {
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
            if all_oleans_present(repo_path, requested_nodes) {
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

    let mut args = vec![repo_path.display().to_string()];
    for node in requested_nodes {
        let cleaned = node.trim();
        if !cleaned.is_empty() {
            args.push("--node".to_string());
            args.push(cleaned.to_string());
        }
    }
    let raw = run_repo_command_json(repo_path, "materialize-tablet-oleans", &args)?;
    let observation: ExternalCommandObservation = serde_json::from_value(raw)
        .map_err(|err| format!("parse materialize-tablet-oleans output failed: {err}"))?;

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

/// Test-only: clear the materialize-tablet-oleans cache. Tests that
/// mutate filesystem state in a single process need this because the
/// static cache otherwise persists across test cases run by `cargo
/// test`'s default parallel runner.
#[cfg(test)]
fn clear_materialize_oleans_cache_for_tests() {
    MATERIALIZE_OLEANS_CACHE.lock().unwrap().clear();
}

fn prepare_compiled_support(repo_path: &Path) -> Result<ExternalCommandObservation, String> {
    let raw = run_repo_command_json(
        repo_path,
        "prepare-compiled-support",
        &[repo_path.display().to_string()],
    )?;
    serde_json::from_value(raw)
        .map_err(|err| format!("parse prepare-compiled-support output failed: {err}"))
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
    let support = sync_tablet_render_support_from_repo(repo_path)?;
    let materialization_nodes: BTreeSet<NodeId> = if requested_nodes.is_empty() {
        present_nodes_from_repo(repo_path)?
    } else {
        requested_nodes.clone()
    };
    let materialized = materialize_tablet_oleans(repo_path, &materialization_nodes)?;
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
        let path = repo
            .join(".lake/build/lib/lean/Tablet")
            .join(format!("{node}.olean"));
        write_test_file(&path, &format!("olean-stub-{node}"));
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
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write_node_olean(&repo, "A");

        clear_materialize_oleans_cache_for_tests();
        let nodes: BTreeSet<NodeId> = [NodeId::from("A")].into_iter().collect();

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
    fn materialize_oleans_cache_invalidates_when_input_lean_changes() {
        let tmp_root = std::env::current_dir().unwrap().join(".tmp-tests");
        fs::create_dir_all(&tmp_root).unwrap();
        let tmp = tempdir_in(&tmp_root).unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        seed_minimal_lake_repo(&repo);
        install_counting_stub_check_script(&repo, &log);
        write_test_file(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write_node_olean(&repo, "A");

        clear_materialize_oleans_cache_for_tests();
        let nodes: BTreeSet<NodeId> = [NodeId::from("A")].into_iter().collect();

        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(count_invocations(&log, "materialize-tablet-oleans"), 1);

        // Worker edits the .lean — content hash shifts ⇒ cache miss.
        write_test_file(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := by trivial\n",
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
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write_node_olean(&repo, "A");

        clear_materialize_oleans_cache_for_tests();
        let nodes: BTreeSet<NodeId> = [NodeId::from("A")].into_iter().collect();

        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(count_invocations(&log, "materialize-tablet-oleans"), 1);

        // Worker hygiene cleanup wipes the olean while .lean stays put.
        fs::remove_file(repo.join(".lake/build/lib/lean/Tablet/A.olean")).unwrap();

        materialize_tablet_oleans(&repo, &nodes).unwrap();
        assert_eq!(
            count_invocations(&log, "materialize-tablet-oleans"),
            2,
            "missing olean must force a fresh materialize-tablet-oleans dispatch"
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
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write_node_olean(&repo, "A");

        clear_materialize_oleans_cache_for_tests();
        let nodes: BTreeSet<NodeId> = [NodeId::from("A")].into_iter().collect();

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

        clear_materialize_oleans_cache_for_tests();
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
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write_test_file(&repo.join("lake-manifest.json"), "{\"version\":1}\n");
        write_node_olean(&repo, "A");

        clear_materialize_oleans_cache_for_tests();
        let nodes: BTreeSet<NodeId> = [NodeId::from("A")].into_iter().collect();

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

    /// Serialise tests that mutate `TRELLIS_KERNEL_CACHE_ROOT`. The env
    /// var is process-global and `cargo test` runs sub-tests in
    /// parallel; without a mutex two disk-cache tests can race and
    /// observe each other's tempdir.
    static DISK_CACHE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_disk_cache_root<R>(cache_root: &Path, body: impl FnOnce() -> R) -> R {
        // Re-acquire on poison: the mutex is here for env-var ordering,
        // not invariant protection — a panic in another disk-cache test
        // would otherwise pin the env var to its tempdir and leak into
        // unrelated tests, but recovering and restoring is enough.
        let _guard = DISK_CACHE_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let prev = std::env::var_os(crate::disk_cache::KERNEL_CACHE_ROOT_ENV);
        // SAFETY: see runtime_cli's `Run` handler — the test binary is
        // single-threaded WRT env mutation while the lock is held.
        unsafe {
            std::env::set_var(crate::disk_cache::KERNEL_CACHE_ROOT_ENV, cache_root);
        }
        let result = body();
        match prev {
            Some(value) => unsafe {
                std::env::set_var(crate::disk_cache::KERNEL_CACHE_ROOT_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(crate::disk_cache::KERNEL_CACHE_ROOT_ENV);
            },
        }
        result
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
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write_node_olean(&repo, "A");

        let nodes: BTreeSet<NodeId> = [NodeId::from("A")].into_iter().collect();

        with_disk_cache_root(&cache_root, || {
            // "Process 1": cold, dispatches once, populates both tiers.
            clear_materialize_oleans_cache_for_tests();
            materialize_tablet_oleans(&repo, &nodes).unwrap();
            assert_eq!(
                count_invocations(&log, "materialize-tablet-oleans"),
                1,
                "cold cache must dispatch once"
            );

            // "Process 2": fresh in-memory cache, but disk persists.
            clear_materialize_oleans_cache_for_tests();
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
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write_node_olean(&repo, "A");

        let nodes: BTreeSet<NodeId> = [NodeId::from("A")].into_iter().collect();

        with_disk_cache_root(&cache_root, || {
            clear_materialize_oleans_cache_for_tests();
            materialize_tablet_oleans(&repo, &nodes).unwrap();
            assert_eq!(count_invocations(&log, "materialize-tablet-oleans"), 1);

            // Worker hygiene cleanup wipes the olean.
            fs::remove_file(repo.join(".lake/build/lib/lean/Tablet/A.olean")).unwrap();
            // New "process": in-memory cleared. Disk cache key would
            // match, but olean-presence guard must force a fresh
            // dispatch.
            clear_materialize_oleans_cache_for_tests();
            materialize_tablet_oleans(&repo, &nodes).unwrap();
            assert_eq!(
                count_invocations(&log, "materialize-tablet-oleans"),
                2,
                "disk-cache hit must still verify olean presence"
            );
        });
    }
}
