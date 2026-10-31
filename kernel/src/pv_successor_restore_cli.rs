// Included by runtime_cli. Explicit, stopped-boundary repair for successors
// created before historical correspondence baselines were retained at genesis.
fn successor_history_exclusions(runtime: &SupervisorRuntime, repo: &Path)
    -> Result<(String, BTreeSet<NodeId>), String>
{
    let git = |args: &[&str]| -> Result<Vec<u8>, String> {
        let out = Command::new("git").arg("--no-replace-objects").arg("-C").arg(repo)
            .args(args).output().map_err(|e| e.to_string())?;
        if !out.status.success() { return Err("cannot read successor checkpoint history".into()); }
        Ok(out.stdout)
    };
    let commits = git(&["log", "--reverse", "--format=%H", "HEAD", "--", ".trellis-history/supervisor_state.json"])?;
    let commit = String::from_utf8(commits).map_err(|e| e.to_string())?
        .lines().next().ok_or("successor has no recorded genesis checkpoint")?.to_string();
    for path in ["modeling-input/revision-from.json", "modeling-input/preserved-dag.json"] {
        if git(&["show", &format!("{commit}:{path}")])? != fs::read(repo.join(path)).map_err(|e| e.to_string())? {
            return Err(format!("successor preservation receipt changed after genesis: {path}"));
        }
    }
    let checkpoint = git(&["show", &format!("{commit}:.trellis-history/supervisor_state.json")])?;
    let value = trellis_kernel::shared_state_codec::decode_shared_state(
        serde_json::from_slice(&checkpoint).map_err(|e| e.to_string())?)?;
    let count = value["event_count"].as_u64().ok_or("checkpoint lacks event count")?;
    let skip = match value.get("event_count_convention") {
        None => count.checked_add(1).ok_or("checkpoint count overflow")?,
        Some(v) if v == "record_count" => count,
        _ => return Err("unsupported checkpoint count convention".into()),
    };
    // Later checkpoints cannot prove that an absent judgment was never revoked.
    if skip > 1 { return Err("successor correspondence repair needs the recorded genesis checkpoint".into()); }
    if skip == 1 {
        let files = trellis_kernel::event_log_cycle_files(&runtime.event_log_dir()).map_err(|e| e.to_string())?;
        let first = files.first().ok_or("successor has no genesis event")?;
        let text = fs::read_to_string(first).map_err(|e| e.to_string())?;
        let record: trellis_kernel::EventLogRecord = serde_json::from_str(
            text.lines().find(|line| !line.trim().is_empty()).ok_or("empty genesis event file")?)
            .map_err(|e| e.to_string())?;
        // Modern parallel genesis checkpoints are written by EnableDispatch;
        // serial genesis uses StartCycle. Neither carries a verifier judgment.
        if record.index != 0 || !matches!(record.event,
            ProtocolEvent::StartCycle | ProtocolEvent::EnableDispatch { .. }) {
            return Err("successor genesis checkpoint skips a substantive event".into());
        }
    }
    let genesis: ProtocolState = serde_json::from_value(value["state"].clone()).map_err(|e| e.to_string())?;
    if genesis.phase != Phase::Modeling || genesis.cycle > 1 {
        return Err("successor checkpoint is not a Modeling genesis".into());
    }
    let scratch = std::env::temp_dir().join(format!("trellis-successor-replay-{}-{}",
        std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?.as_nanos()));
    fs::create_dir(&scratch).map_err(|e| e.to_string())?;
    let result: Result<BTreeSet<NodeId>, String> = (|| {
        let seed = scratch.join("seed.json");
        let output = scratch.join("replayed.json");
        fs::write(&seed, checkpoint).map_err(|e| e.to_string())?;
        let mut touched = BTreeSet::new();
        // Use the ordinary native replay, including archived request hydration,
        // certificate carriers, migrations and legitimate stale-event handling.
        replay_to_event_count_observed(runtime.paths().root.clone(), runtime.event_count(),
            Some(output.clone()), Some(seed), Some(&mut |before, after| {
                touched.extend(trellis_kernel::successor_correspondence::touched(before, after));
            }))?;
        let replay: ProtocolState = serde_json::from_slice(&fs::read(output).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        if replay.corr_status != runtime.state().corr_status
            || replay.corr_approved_fingerprints != runtime.state().corr_approved_fingerprints
            || replay.live.present_nodes != runtime.state().live.present_nodes
            || replay.node_kinds != runtime.state().node_kinds || replay.node_role != runtime.state().node_role
            || replay.target_claims != runtime.state().target_claims
            || replay.challenge_claims != runtime.state().challenge_claims
        {
            return Err("successor judgment history does not reproduce the current checkpoint".into());
        }
        Ok(touched)
    })();
    let _ = fs::remove_dir_all(&scratch);
    let touched = result?;
    Ok((commit, touched))
}

fn restore_successor_correspondence(root: &Path, apply: bool) -> Result<Value, String> {
    use std::os::fd::AsRawFd;
    let mut locks = Vec::new();
    if apply {
        for name in ["owner.lock", "coordinator.lock"] {
            let lock = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
                .open(root.join("dispatch").join(name)).map_err(|e| e.to_string())?;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err("stop the parallel coordinator before successor correspondence restoration".into());
            }
            locks.push(lock);
        }
    }
    if root.join("dispatch/publication/pending.json").exists() {
        return Err("finish pending publication before successor correspondence restoration".into());
    }
    let mut runtime = SupervisorRuntime::load(RuntimePaths::new(root)).map_err(|e| e.to_string())?;
    let repo = runtime.metadata().repo_path.clone().ok_or("successor restoration requires a repository")?;
    let _owners = if apply { Some(pv_revision_activation_ownership(root, Some(&repo))?) } else { None };
    let (history_checkpoint, touched) = successor_history_exclusions(&runtime, &repo)?;
    let restoration = derive_pv_successor_correspondence_baselines(runtime.state(), &repo, &touched)?;
    let receipt = successor_read_json(&root.join("pv-successor-preservation.json"))?;
    if receipt["package_digest"] != restoration.package_digest
        || receipt["predecessor_state_sha256"] != restoration.predecessor_state_sha256
    { return Err("successor restoration differs from the recorded preservation receipt".into()); }
    let matching: BTreeSet<_> = restoration.baselines.iter()
        .filter(|(node, fp)| runtime.state().live.corr_current_fingerprints.get(*node) == Some(*fp))
        .map(|(node, _)| node.clone()).collect();
    let output = json!({"applied": apply && !restoration.baselines.is_empty(), "baselines": restoration.baselines.keys().collect::<Vec<_>>(),
        "matching_current_observation": matching, "excluded": restoration.exclusions,
        "history_checkpoint": history_checkpoint, "package_digest": restoration.package_digest});
    if apply && !restoration.baselines.is_empty() {
        require_runtime_identity(&runtime)?;
        verify_prose_referent_for_step(&runtime)?;
        let payload = trellis_kernel::successor_correspondence::Restoration {
            state_sha256: trellis_kernel::dispatch_integration::state_digest(runtime.state())?,
            package_digest: restoration.package_digest,
            predecessor_state_sha256: restoration.predecessor_state_sha256,
            history_checkpoint, baselines: restoration.baselines,
        };
        let mut sink = checkpoint_sink_from_env()?.ok_or("successor restoration requires TRELLIS_RUNTIME_CHECKPOINT_HOOK")?;
        runtime.step_injected_event_with_checkpoint_sink(ProtocolEvent::RestoreSuccessorCorrespondence { payload }, &mut sink)
            .map_err(|e| format!("successor restoration refused: {e}"))?;
    }
    Ok(output)
}
