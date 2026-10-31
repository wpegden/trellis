# Frozen validator from 3a8c24f8:trellis/checker/server.py for differential tests.
def _try_local_closure_axioms_cache_hit(
    self, request: CheckerRequest
) -> Optional[Tuple[Mapping[str, Any], Any, Mapping[str, Any]]]:
    """Cache-hit-only branch of ``_handle_local_closure_axioms``: derive
    the cache key, attempt a load, return a synth response if it hits.
    Returns ``None`` on miss or unkeyed (caller falls through to lake
    under ``_workspace_lock``). Pattern mirrors
    ``_try_print_axioms_cache_hit`` — both ops share the same closure-
    walked cache key surface and benefit equally from bypassing the
    workspace lock on a cache hit.

    Like every cache-hit path, this one performs no elaboration and must
    never carry an elaboration-cost measurement. Doubly excluded here:
    ``local_closure_axioms`` imports an already-built olean rather than
    producing one, so even its lake-spawning miss path takes no
    measurement (it calls ``_run_lake_command`` without a ``metrics``
    out-dict).
    """
    rid = request.request_id
    node_name = request.node_name
    assert node_name is not None

    # Scan-only requests are never served from this cache. An earlier
    # synthesis inferred a scan-only success from any cached full-probe
    # success, on the premise that the full probe runs the same
    # owner-file scan (``ownerFileScanRejection``) before its closure
    # walk. That premise no longer holds: the Lean-side refactor
    # (865274c3) replaced the scan with ``ownerFilePolicyScan``, whose
    # only call site is inside ``runScanOnly`` — the full certificate
    # probe never runs it — so a stored full-probe ``status="ok"`` does
    # not prove the owner-file policy scan passed. The scan is the gate
    # that rejects declaration-forging command kinds (``macro``,
    # ``elab``, ``syntax``, ``run_cmd``, ...), which elaborate cleanly
    # and earn a full-probe ``ok``. Fall through to the live, parse-only
    # scan unconditionally (the pre-reuse behavior; fail-closed).
    scan_only = bool(request.raw.get("scan_only", False))
    if scan_only:
        return None
    # Serving the FULL cached result replays closure facts about olean
    # artifacts, so it additionally requires a current kernel-replay
    # attestation for the exact olean closure (observation cache versions
    # describe record shape, not replay state).
    if not self._closures_have_current_kernel_replay([node_name]):
        return None

    script_sha = _sha256_file_or_empty(self._local_closure_script_path)
    toolchain_sha = _sha256_file_or_empty(self._toolchain_path)
    manifest_sha = _sha256_file_or_empty(self._lake_manifest_path)
    if not (script_sha and toolchain_sha):
        return None
    sync_cache = load_fingerprint_cache(self.fingerprint_cache_path)
    try:
        cache_key_base = compute_semantic_payload_cache_key(
            self.supervisor_repo,
            node_name,
            sync_cache,
            script_sha,
            toolchain_sha,
            manifest_sha,
            LOCAL_CLOSURE_AXIOMS_CACHE_VERSION,
        )
    except Exception:
        _LOGGER.exception(
            "compute_semantic_payload_cache_key failed for local_closure_axioms %s",
            node_name,
        )
        return None
    if cache_key_base is None:
        return None

    no_axcheck = bool(request.raw.get("no_axcheck", False))
    module_owner = bool(request.raw.get("module_owner", False))
    principal_name = request.raw.get("principal_name")
    if module_owner:
        if node_name != "Preamble" or principal_name is not None:
            return None
        principal_identity = "<module-owner>"
    else:
        if not isinstance(principal_name, str) or not principal_name:
            return None
        principal_identity = principal_name
    principal_key = hashlib.sha256(principal_identity.encode("utf-8")).hexdigest()
    cache_key = f"{cache_key_base}-principal-{principal_key}"
    if no_axcheck:
        cache_key += "-noax"

    try:
        sidecar = load_local_closure_axioms(
            self.local_closure_axioms_cache_dir,
            cache_key,
            expected_version=LOCAL_CLOSURE_AXIOMS_CACHE_VERSION,
        )
    except Exception:
        _LOGGER.exception(
            "load_local_closure_axioms failed for %s/%s", node_name, cache_key
        )
        return None
    if sidecar is None or not isinstance(sidecar.get("response"), dict):
        return None
    cached_response = dict(sidecar["response"])
    # Refresh the per-request request_id so the response echoes this
    # request's id, not the one stored.
    cached_response["request_id"] = rid
    return (
        cached_response,
        cached_response.get("returncode"),
        {"local_closure_axioms_cache_hit": True},
    )
