// Included in engine's test module to reuse its ordinary routing fixtures.
fn sound_wakeup_review(mut state: ProtocolState, inputs: &BTreeMap<String, String>,
    enabled: bool, later_event: bool) -> ProtocolState {
    use crate::dispatch_routing::RoutingEvent;
    let id = state.in_flight_request.as_ref().map(|r| r.id).unwrap_or(state.request_seq + 1);
    state = apply_event(state, ProtocolEvent::Dispatch { payload:
        crate::dispatch::DispatchEvent::ReviewStarted { request_id: id } }).unwrap().state;
    // Models a worker/result event arriving after the reviewer snapshot.
    if later_event { state.dispatch.as_mut().unwrap().signal().unwrap(); }
    let mut response = verifier_only_theorem_continue(id, state.cycle);
    response.comments = "Preserve the worker; wait for requested Sound".into();
    let state = parallel_routing(state, RoutingEvent::ReviewAction {
        review_id: id, response_sha256: "1".repeat(64), response: Box::new(response),
    });
    // The accepted action survives a restart before its Route event.
    let state: ProtocolState = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
    parallel_routing(state, RoutingEvent::Route { inputs: inputs.clone(),
        verifier_context_version: 5, suppress_sound_review_self_wakeup: enabled })
}

#[test]
fn parallel_dispatch_sound_review_self_wakeup_is_suppressed_without_losing_later_events() {
    std::thread::Builder::new().stack_size(64 * 1024 * 1024).spawn(|| {
    let (mut initial, _) = theorem_verifier_only_review_state();
    initial.theorem_verifier_only_continuation_enabled = true;
    initial.dispatch = Some(crate::dispatch::DispatchLedger::new(Default::default(), initial.phase).unwrap());
    crate::runtime::normalize_in_flight_request_execution_hints(&mut initial);
    let inputs = BTreeMap::from([("Tablet/a.lean".into(), "1".repeat(64))]);
    for enabled in [false, true] {
        for later_event in [false, true] {
            let mut state = sound_wakeup_review(initial.clone(), &inputs, enabled, later_event);
            let ledger = state.dispatch.as_mut().unwrap();
            assert_eq!(ledger.verifiers.jobs.len(), 1);
            assert_eq!(ledger.reviewer_runnable(), !enabled || later_event);
            assert_eq!(ledger.start_review(state.request_seq + 1).is_ok(), !enabled || later_event,
                "native admission agrees with status and does not swallow a later event");
        }
    }
    }).unwrap().join().unwrap();
}

#[test]
fn parallel_dispatch_sound_review_self_wakeup_same_queued_running_checking_job() {
    use crate::dispatch_routing::RoutingEvent;
    std::thread::Builder::new().stack_size(64 * 1024 * 1024).spawn(|| {
    let (mut initial, _) = theorem_verifier_only_review_state();
    initial.theorem_verifier_only_continuation_enabled = true;
    initial.dispatch = Some(crate::dispatch::DispatchLedger::new(Default::default(), initial.phase).unwrap());
    crate::runtime::normalize_in_flight_request_execution_hints(&mut initial);
    let inputs = BTreeMap::from([("Tablet/a.lean".into(), "1".repeat(64))]);
    // Reproduce the legacy first wake: a second review sees an existing job.
    let queued = sound_wakeup_review(initial, &inputs, false, false);
    let job = queued.dispatch.as_ref().unwrap().verifiers.jobs.values().next().unwrap();
    let id = job.id;
    let request = job.request.as_ref().unwrap();
    let response = WrapperResponse::Sound(SoundResponse { request_id: request.id, cycle: request.cycle,
        status: ResponseStatus::Ok, lane_updates: unanimous_sound_lanes(&request.verify_lanes, "a", SoundStatus::Pass),
        reviewer_evidence: BTreeMap::new() });
    let running = parallel_routing(queued.clone(), RoutingEvent::VerifierLaunched { id,
        process: crate::dispatch::ProcessIdentity { pid: 10, start_ticks: 1, boot_id: "test".into() }, now_ms: 1 });
    let checking = parallel_routing(running.clone(), RoutingEvent::VerifierCandidate { id, response: Box::new(response.clone()) });
    for state in [queued, running, checking] {
        let original_job = state.dispatch.as_ref().unwrap().verifiers.jobs[&id].clone();
        let state = sound_wakeup_review(state, &inputs, true, false);
        let ledger = state.dispatch.as_ref().unwrap();
        assert_eq!(ledger.verifiers.jobs.len(), 1, "share the original job, do not launch again");
        assert_eq!(ledger.verifiers.jobs[&id].identity, original_job.identity);
        assert_eq!(ledger.verifiers.jobs[&id].status, original_job.status);
        assert!(!ledger.reviewer_runnable(), "the accepted request cannot wake its own replacement");
        let replayed = parallel_routing(state.clone(), RoutingEvent::Route { inputs: inputs.clone(),
            verifier_context_version: 5, suppress_sound_review_self_wakeup: true });
        assert_eq!(replayed, state, "an idle route cannot manufacture another cycle");
        if matches!(original_job.status, crate::dispatch_verifiers::VerifierStatus::Checking { .. }) {
            let finish = RoutingEvent::VerifierFinished { id, inputs: inputs.clone(), response: Box::new(response.clone()) };
            let done = parallel_routing(state, finish.clone());
            assert!(done.dispatch.as_ref().unwrap().reviewer_runnable(), "the actual verifier result still wakes review");
            assert!(done.current_sound_pass(&NodeId::from("a")));
            assert_eq!(parallel_routing(done.clone(), finish), done, "result replay remains idempotent");
        }
    }
    }).unwrap().join().unwrap();
}
