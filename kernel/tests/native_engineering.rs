use trellis_kernel::{engine::{apply_event, ProtocolEvent}, engineering::*, model::*, worker_attempt as wa};

fn id(n: u8) -> String { format!("{n:064x}") }
fn profile(purpose: Purpose) -> Profile {
    Profile { provider: "fixture".into(), model: if purpose == Purpose::Implementation { "implementation-model" } else { "review-model" }.into(),
        reasoning_effort: "xhigh".into(), allowance_ms: 1_000 }
}
fn initial() -> ProtocolState {
    initialize(Subject { route: trellis_kernel::paper_implementation::PaperImplementationRoute::RustAeneas, paper_sha256: id(1), targets_sha256: id(2), contract_sha256: id(3), notes_sha256: Some(id(4)),
        options_sha256: id(5), tools_sha256: id(6) }, [Purpose::SourceReading, Purpose::Plan, Purpose::PlanReview,
            Purpose::Implementation, Purpose::CodeReview, Purpose::Performance].into_iter().map(|p|(p,profile(p))).collect(), wa::Policy::default()).unwrap()
}
fn event(state: ProtocolState, event: ProtocolEvent) -> ProtocolState { apply_event(state, event).unwrap().state }
fn issue(state: ProtocolState) -> ProtocolState { event(state, ProtocolEvent::StartCycle) }
fn start(state: ProtocolState) -> ProtocolState {
    let id = state.request_seq; event(state, ProtocolEvent::WorkerAttemptStarted { request_id: id, allowance_ms: 1000 })
}
fn process() -> trellis_kernel::dispatch::ProcessIdentity { trellis_kernel::dispatch::ProcessIdentity { pid: 11, start_ticks: 10, boot_id: "fixture-boot".into() } }
fn observed(state: ProtocolState, observation: wa::Event) -> ProtocolState {
    let request_id = state.request_seq; event(state, ProtocolEvent::WorkerAttemptObserved { request_id, observation })
}
fn launch(state: ProtocolState) -> ProtocolState { observed(state, wa::Event::Launched { process: process(), started_at_ms: 10 }) }
fn complete(state: ProtocolState, elapsed: u64) -> ProtocolState {
    let attempt = &state.worker_attempt.as_ref().unwrap().attempt;
    let c = &state.in_flight_request.as_ref().unwrap().engineering.as_ref().unwrap();
    let completion = wa::Completion { process: process(), turn: attempt.turn, elapsed_ms: elapsed, exhausted: false,
        response_sha256: id(9), session: wa::Session { provider: c.profile.provider.clone(), model: c.profile.model.clone(),
            reasoning_effort: c.profile.reasoning_effort.clone(), thread_id: format!("assignment-{}", state.request_seq), home: format!("/fixture/home/{}",state.request_seq) } };
    observed(state, wa::Event::TurnCompleted { completion })
}
fn seal(state: ProtocolState) -> ProtocolState { observed(state, wa::Event::Sealed { candidate_sha256: id(10) }) }
fn submitted(state: ProtocolState) -> ProtocolState { seal(complete(launch(start(issue(state))), 100)) }
fn checked(state: &ProtocolState, outcome: Outcome) -> Checked {
    Checked { request_id: state.request_seq, cycle: state.cycle, request_sha256: hash(&**state.in_flight_request.as_ref().unwrap()),
        turn: state.worker_attempt.as_ref().unwrap().attempt.turn, candidate_sha256: id(10),
        basis_sha256: state.engineering.as_ref().unwrap().basis(), host_receipt_sha256: id(11), outcome }
}
fn accept(state: ProtocolState, outcome: Outcome) -> ProtocolState {
    let checked = Box::new(checked(&state, outcome)); event(state, ProtocolEvent::EngineeringOutcome { checked })
}
fn plan() -> Plan { Plan { requirements: vec![PlanEntry { source: "paper section".into(), description: "required behavior".into(), consequences: "test behavior".into() }], interpretations: vec![], unspecified: vec![], departures: vec![] } }
fn record() -> Outcome { Outcome::Record { summary: "Read the supplied source".into(), record: plan() } }
fn review(decision: Decision) -> Outcome { Outcome::Review { decision, reason: "independent review".into() } }
fn planned() -> ProtocolState { accept(submitted(accept(submitted(accept(submitted(initial()), record())), record())), review(Decision::Accept)) }
fn candidate(state: &ProtocolState, source: u8) -> Candidate {
    let mut c = Candidate { source_sha256: id(source), build_sha256: id(22), lock_sha256: id(23), tools_sha256: id(6),
        plan_sha256: state.engineering.as_ref().unwrap().plan_sha256.clone().unwrap(), checks: vec![] };
    c.checks = ["check", "test"].into_iter().map(|v| Check { argv: ["cargo",v,"--locked","--offline","--all-targets"].into_iter().map(String::from).collect(),
        exit_code: 0, basis_sha256: c.basis(), receipt_sha256: id(24) }).collect(); c
}
fn implemented(state: ProtocolState, source: u8) -> ProtocolState {
    let c = candidate(&state, source); accept(submitted(state), Outcome::Implementation { summary: "Implemented current plan".into(), candidate: c })
}
fn reviewed(state: ProtocolState, source: u8) -> ProtocolState { accept(submitted(implemented(state,source)), review(Decision::Accept)) }
fn malformed() -> Outcome { Outcome::Malformed { diagnostics: vec![Diagnostic { path: "reading.json".into(), rule: DiagnosticRule::ClosedFields }] } }
fn exhausted(state: ProtocolState) -> ProtocolState {
    let state = issue(state); let state = start(state); seal(complete(launch(state), 1000))
}

#[test] fn native_engineering_initial_subject_is_pre_extraction_and_notes_remain_separate() {
    let state = initial(); state.validate().unwrap();
    assert!(state.paper_implementation.is_none()); assert!(state.live.present_nodes.is_empty());
    assert!(!state.modeling_human_approved);
    let request_state = issue(state); request_state.validate().unwrap();
    let request = request_state.in_flight_request.as_ref().unwrap();
    assert_eq!(request.kind,RequestKind::Engineering); assert!(request.fresh_context);
    assert_eq!(request.engineering.as_ref().unwrap().subject.notes_sha256, Some(id(4)));
    assert!(require_dispatch_supported(request).is_err());
    assert_eq!(request.engineering.as_ref().unwrap().purpose,Purpose::SourceReading);
    assert!(request.project_invariants.as_object().is_some_and(|v|v.is_empty()));
}
#[test] fn native_engineering_requests_have_exact_role_pins_and_monotone_ids() {
    let state = submitted(initial()); let id = state.request_seq;
    let state = issue(accept(state, record()));
    assert_eq!(state.request_seq, id+1); assert_eq!(state.engineering.as_ref().unwrap().purpose,Purpose::Plan);
    let state = issue(planned()); assert_eq!(state.in_flight_request.as_ref().unwrap().engineering.as_ref().unwrap().profile,profile(Purpose::Implementation));
}
#[test] fn native_engineering_closed_records_and_nonempty_fields() {
    let mut value = serde_json::to_value(plan()).unwrap(); value["assessment"] = "extra".into(); assert!(serde_json::from_value::<Plan>(value).is_err());
    let mut p=plan(); p.requirements[0].consequences=" \n".into(); assert!(p.validate().is_err());
    p=plan(); p.requirements.clear(); assert!(p.validate().is_err());
}
#[test] fn native_engineering_formal_events_and_gates_are_inadmissible() {
    let mut state=initial(); state.modeling_human_approved=true; assert!(state.validate().is_err());
    let mut state=initial(); state.stage=Stage::HumanGate; assert!(state.validate().is_err());
    assert!(apply_event(initial(),ProtocolEvent::ActivatePvRevision).is_err());
    let mut state=initial(); state.phase=Phase::TheoremStating; assert!(state.validate().is_err());
    let mut state=initial(); state.configured_targets.insert(TargetId::from("target")); assert!(state.validate().is_err());
}
#[test] fn native_engineering_missing_review_or_departure_cannot_advance() {
    let state=submitted(initial()); let result=checked(&state,review(Decision::Accept));
    assert!(apply_event(state,ProtocolEvent::EngineeringOutcome{checked:Box::new(result)}).is_err());
    let mut state=planned(); state.engineering.as_mut().unwrap().plan_approval_sha256=None;
    assert!(apply_event(state,ProtocolEvent::StartCycle).is_err());
    let state=accept(submitted(planned()),Outcome::Propose {reason:"New implementation choice".into()});
    assert_eq!(state.engineering.as_ref().unwrap().purpose,Purpose::Plan);
    assert!(state.engineering.as_ref().unwrap().plan_approval_sha256.is_none());
    assert!(state.engineering.as_ref().unwrap().candidate.is_none());
}
#[test] fn native_engineering_rejected_plan_returns_to_planning() {
    let state=accept(submitted(accept(submitted(initial()),record())),record());
    let state=accept(submitted(state),review(Decision::Reject));
    assert_eq!(state.engineering.as_ref().unwrap().purpose,Purpose::Plan);
    assert!(state.engineering.as_ref().unwrap().plan_approval_sha256.is_none());
}
#[test] fn native_engineering_source_and_check_freshness_required_before_review() {
    let state=submitted(planned());
    for mutation in 0..5 {
        let mut c=candidate(&state,20);
        match mutation {0=>c.checks[0].exit_code=1,1=>c.source_sha256=id(25),2=>c.tools_sha256=id(26),3=>c.checks.pop().map(|_|()).unwrap(),_=>c.plan_sha256=id(27)}
        let ch=checked(&state,Outcome::Implementation{summary:"changes".into(),candidate:c});
        assert!(apply_event(state.clone(),ProtocolEvent::EngineeringOutcome{checked:Box::new(ch)}).is_err());
    }
}
#[test] fn native_engineering_stale_request_input_profile_and_result_refused() {
    let state=submitted(initial());
    for mutation in 0..5 {
        let mut st=state.clone(); let mut ch=checked(&st,record());
        match mutation {0=>ch.request_id+=1,1=>ch.turn+=1,2=>ch.basis_sha256=id(33),3=>st.engineering.as_mut().unwrap().subject.notes_sha256=None,_=>st.in_flight_request.as_mut().unwrap().engineering.as_mut().unwrap().profile.model="other".into()}
        assert!(apply_event(st,ProtocolEvent::EngineeringOutcome{checked:Box::new(ch)}).is_err());
    }
}
#[test] fn native_engineering_same_session_correction_uses_original_budget_and_exact_feedback() {
    let st=submitted(initial()); let ch=checked(&st,malformed()); let expected=hash(&ch.outcome);
    let st=event(st,ProtocolEvent::EngineeringChecked{checked:Box::new(ch)});
    let attempt=&st.worker_attempt.as_ref().unwrap().attempt;
    assert_eq!(attempt.turn,2); assert_eq!(attempt.remaining_ms(),900);
    assert_eq!(attempt.phase,wa::Phase::Ready{feedback_sha256:Some(expected)});
    let st=seal(complete(launch(st),100)); let ch=checked(&st,record());
    let done=event(st.clone(),ProtocolEvent::EngineeringOutcome{checked:Box::new(ch.clone())});
    assert!(apply_event(done.clone(),ProtocolEvent::EngineeringOutcome{checked:Box::new(ch)}).is_err());
    assert_eq!(done.engineering.as_ref().unwrap().last_consumption.as_ref().unwrap().turn,2);
}
#[test] fn native_engineering_wrong_thread_model_and_premature_finished_refused() {
    let st=launch(start(issue(initial())));
    assert!(apply_event(st.clone(),ProtocolEvent::WorkerAttemptObserved{request_id:st.request_seq,observation:wa::Event::Finished}).is_err());
    let ch=Checked{request_id:st.request_seq,cycle:st.cycle,request_sha256:hash(&**st.in_flight_request.as_ref().unwrap()),turn:1,candidate_sha256:id(10),basis_sha256:st.engineering.as_ref().unwrap().basis(),host_receipt_sha256:id(11),outcome:record()};
    assert!(apply_event(st,ProtocolEvent::EngineeringOutcome{checked:Box::new(ch)}).is_err());
    let mut st=submitted(initial());let ch=checked(&st,malformed());st=event(st,ProtocolEvent::EngineeringChecked{checked:Box::new(ch)});st=launch(st);
    let mut bad=st.worker_attempt.as_ref().unwrap().attempt.session.clone().unwrap();bad.thread_id="other".into();
    let completion=wa::Completion{process:process(),turn:2,elapsed_ms:10,exhausted:false,response_sha256:id(9),session:bad};
    assert!(apply_event(st.clone(),ProtocolEvent::WorkerAttemptObserved{request_id:st.request_seq,observation:wa::Event::TurnCompleted{completion}}).is_err());
}
#[test] fn native_engineering_interruption_never_uses_transport_retry_allowance() {
    let st=launch(start(issue(initial())));
    let interruption=wa::Interruption{process:process(),turn:1,elapsed_ms:10,session:wa::Session{provider:"fixture".into(),model:"review-model".into(),reasoning_effort:"xhigh".into(),thread_id:"thread".into(),home:"/fixture".into()},evidence_sha256:id(1)};
    let st=observed(st,wa::Event::Interrupted{interruption});
    assert_eq!(st.engineering.as_ref().unwrap().halt,Some(Halt::Interrupted));
    assert!(apply_event(st.clone(),ProtocolEvent::StartCycle).is_err());
    assert!(apply_event(st.clone(),ProtocolEvent::WorkerAttemptObserved{request_id:st.request_seq,observation:wa::Event::Launched{process:process(),started_at_ms:20}}).is_err());
    let restored:ProtocolState=serde_json::from_slice(&serde_json::to_vec(&st).unwrap()).unwrap();
    assert!(apply_event(restored,ProtocolEvent::StartCycle).is_err());
}
#[test] fn native_engineering_safe_exhaustion_uses_bounded_outer_retry_then_independent_review() {
    let st=accept(exhausted(initial()),malformed());assert_eq!(st.engineering.as_ref().unwrap().purpose,Purpose::SourceReading);
    let st=accept(exhausted(st),malformed());assert_eq!(st.engineering.as_ref().unwrap().purpose,Purpose::RetryReview);
    assert!(st.engineering.as_ref().unwrap().reading_sha256.is_none());
    let request=issue(st.clone());assert_eq!(request.in_flight_request.as_ref().unwrap().engineering.as_ref().unwrap().profile,profile(Purpose::PlanReview));
    let st=accept(submitted(st),review(Decision::Revise));assert_eq!(st.engineering.as_ref().unwrap().purpose,Purpose::SourceReading);
    assert!(st.engineering.as_ref().unwrap().reading_sha256.is_none());assert!(st.engineering.as_ref().unwrap().retry_review.is_none());
    issue(st).validate().unwrap();
}
#[test] fn native_engineering_retry_review_cannot_accept_or_advance_or_change_basis() {
    let st=accept(exhausted(accept(exhausted(initial()),malformed())),malformed());
    let st=submitted(st);let ch=checked(&st,review(Decision::Accept));assert!(apply_event(st.clone(),ProtocolEvent::EngineeringOutcome{checked:Box::new(ch)}).is_err());
    let mut bad=st.clone();bad.engineering.as_mut().unwrap().retry_review.as_mut().unwrap().basis_sha256=id(90);assert!(bad.validate().is_err());
    let failed=accept(st,review(Decision::Fail));assert!(failed.engineering.as_ref().unwrap().failed);assert_ne!(failed.stage,Stage::HumanGate);assert!(apply_event(failed,ProtocolEvent::StartCycle).is_err());
}
#[test] fn native_engineering_malformed_retry_review_does_not_refill_failed_assignment() {
    let st=accept(exhausted(accept(exhausted(initial()),malformed())),malformed());
    let st=accept(exhausted(st),malformed());assert!(st.engineering.as_ref().unwrap().failed);
    assert_eq!(st.engineering.as_ref().unwrap().purpose,Purpose::RetryReview);
    assert!(apply_event(st,ProtocolEvent::StartCycle).is_err());
}
#[test] fn native_engineering_best_measured_reviewed_candidate_is_retained() {
    let st=reviewed(planned(),20);let first=hash(st.engineering.as_ref().unwrap().candidate.as_ref().unwrap());let retained=st.engineering.as_ref().unwrap().current_reviewed_candidate_sha256.clone().unwrap();
    let m=Measurement{candidate_sha256:first.clone(),reviewed_candidate_sha256:st.engineering.as_ref().unwrap().current_reviewed_candidate_sha256.clone().unwrap(),harness_sha256:id(55),record_sha256:id(56),exit_codes:vec![0]};
    let st=accept(submitted(st),Outcome::Measure{reason:"representative measurement".into(),measurement:m});
    let before=st.engineering.as_ref().unwrap().clone();
    let st=accept(submitted(st),Outcome::Improve{reason:"dominant cost".into(),instruction:"evaluate practical improvement".into()});
    let improved=st.engineering.as_ref().unwrap();
    assert_eq!(improved.purpose,Purpose::Implementation);assert_eq!(improved.generation,before.generation);
    assert_eq!(improved.plan_approval_sha256,before.plan_approval_sha256);assert_eq!(improved.candidate,before.candidate);
    assert_eq!(improved.reviewed,before.reviewed);assert_eq!(improved.measurements,before.measurements);
    assert!(improved.current_reviewed_candidate_sha256.is_none());
    let attempted=submitted(st.clone());let ch=checked(&attempted,Outcome::Measure{reason:"unreviewed".into(),measurement:before.measurements[&id(56)].clone()});
    assert!(apply_event(attempted,ProtocolEvent::EngineeringOutcome{checked:Box::new(ch)}).is_err());
    let st=reviewed(st,21);assert_ne!(hash(st.engineering.as_ref().unwrap().candidate.as_ref().unwrap()),first);
    let baseline=Baseline{harness_sha256:id(55),reviewed_candidate_sha256:retained.clone(),assessment_sha256:id(60),measurement_sha256:vec![id(56)],selection_receipt_sha256:id(61)};
    let st=accept(submitted(st),Outcome::Accept{reason:"earlier correct candidate measured better".into(),baseline});
    assert_eq!(st.stage,Stage::EngineeringReady);assert_eq!(hash(st.engineering.as_ref().unwrap().candidate.as_ref().unwrap()),first);
    assert!(!st.modeling_human_approved);assert!(st.paper_implementation.is_none());st.validate().unwrap();
    assert!(apply_event(st,ProtocolEvent::StartCycle).is_err());
}
#[test] fn native_engineering_failed_or_unrecorded_measurement_cannot_make_ready() {
    let st=reviewed(planned(),20);let baseline=Baseline{harness_sha256:id(55),reviewed_candidate_sha256:st.engineering.as_ref().unwrap().current_reviewed_candidate_sha256.clone().unwrap(),assessment_sha256:id(60),measurement_sha256:vec![id(56)],selection_receipt_sha256:id(61)};
    let st=submitted(st);let ch=checked(&st,Outcome::Accept{reason:"accept".into(),baseline});assert!(apply_event(st,ProtocolEvent::EngineeringOutcome{checked:Box::new(ch)}).is_err());
}
#[test] fn native_engineering_legacy_serial_state_and_contract_bytes_remain_compatible() {
    let state=ProtocolState::default();assert!(!serde_json::to_value(&state).unwrap().as_object().unwrap().contains_key("engineering"));
    let request=WrapperRequest::default();assert!(!serde_json::to_value(&request).unwrap().as_object().unwrap().contains_key("engineering"));
    assert!(require_dispatch_supported(&request).is_ok());
    let restored:ProtocolState=serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();assert_eq!(restored,state);
}

#[test] fn native_engineering_submission_cap_uses_three_turns_then_safe_outer_retry() {
    let mut st=submitted(initial());
    for expected in [2,3] {
        let ch=checked(&st,malformed());st=event(st,ProtocolEvent::EngineeringChecked{checked:Box::new(ch)});
        assert_eq!(st.worker_attempt.as_ref().unwrap().attempt.turn,expected);
        st=seal(complete(launch(st),100));
    }
    assert!(!st.worker_attempt.as_ref().unwrap().attempt.can_correct());
    let ch=checked(&st,malformed());assert!(apply_event(st.clone(),ProtocolEvent::EngineeringChecked{checked:Box::new(ch)}).is_err());
    let st=accept(st,malformed());assert_eq!(st.engineering.as_ref().unwrap().purpose,Purpose::SourceReading);
    assert_eq!(st.engineering.as_ref().unwrap().last_consumption.as_ref().unwrap().turn,3);
}
#[test] fn native_engineering_plan_edits_invalidate_approval_at_load() {
    let mut st=planned();st.engineering.as_mut().unwrap().plan_sha256=Some(id(99));assert!(st.validate().is_err());
    let mut st=planned();st.engineering.as_mut().unwrap().generation+=1;assert!(st.validate().is_err());
}
#[test] fn native_engineering_output_boundaries_match_existing_delivery_purposes() {
    let implementation=issue(planned());assert_eq!(implementation.in_flight_request.as_ref().unwrap().engineering.as_ref().unwrap().output_paths,["source"]);
    assert_eq!(implementation.in_flight_request.as_ref().unwrap().engineering.as_ref().unwrap().artifact_contract["record_path"],"plan.json");
    let performance=issue(reviewed(planned(),20));assert_eq!(performance.in_flight_request.as_ref().unwrap().engineering.as_ref().unwrap().output_paths,["benchmarks/harness","benchmarks/report.md"]);
}
#[test] fn native_engineering_ready_runtime_waits_on_repeated_entry_without_formal_handoff() {
    use trellis_kernel::{SupervisorRuntime,RuntimePaths,RuntimeStepStatus,WrapperAdapter};
    struct NoDispatch;
    impl WrapperAdapter for NoDispatch { fn dispatch(&mut self,_:&WrapperRequest)->Result<WrapperResponse,String>{panic!("ready must wait")} }
    let st=reviewed(planned(),20);let first=hash(st.engineering.as_ref().unwrap().candidate.as_ref().unwrap());let retained=st.engineering.as_ref().unwrap().current_reviewed_candidate_sha256.clone().unwrap();
    let m=Measurement{candidate_sha256:first.clone(),reviewed_candidate_sha256:st.engineering.as_ref().unwrap().current_reviewed_candidate_sha256.clone().unwrap(),harness_sha256:id(55),record_sha256:id(56),exit_codes:vec![0]};
    let st=accept(submitted(st),Outcome::Measure{reason:"measurement".into(),measurement:m});
    let baseline=Baseline{harness_sha256:id(55),reviewed_candidate_sha256:retained,assessment_sha256:id(60),measurement_sha256:vec![id(56)],selection_receipt_sha256:id(61)};
    let st=accept(submitted(st),Outcome::Accept{reason:"satisfactory measured candidate".into(),baseline});
    let dir=tempfile::tempdir().unwrap();let mut runtime=SupervisorRuntime::initialize(RuntimePaths::new(dir.path()),st).unwrap();
    for _ in 0..2 {let outcome=runtime.step(&mut NoDispatch).unwrap();assert_eq!(outcome.status,RuntimeStepStatus::EngineeringReady);assert!(outcome.event.is_none());assert!(outcome.commands.is_empty());}
    assert!(!runtime.state().modeling_human_approved);assert!(runtime.state().paper_implementation.is_none());
}

#[test] fn native_engineering_runtime_refuses_unowned_terminal_sink() {
    use trellis_kernel::{SupervisorRuntime,RuntimePaths,WorkerAttemptObserver,WrapperAdapter};
    struct ReadingAdapter;
    impl WrapperAdapter for ReadingAdapter {
        fn dispatch(&mut self,_:&WrapperRequest)->Result<WrapperResponse,String>{panic!("engineering uses shared observer")}
        fn dispatch_with_worker_observer(&mut self, request:&WrapperRequest, observer:&mut dyn WorkerAttemptObserver)->Result<WrapperResponse,String>{
            observer.observe_worker_attempt(ProtocolEvent::WorkerAttemptStarted{request_id:request.id,allowance_ms:1000})?;
            observer.observe_worker_attempt(ProtocolEvent::WorkerAttemptObserved{request_id:request.id,observation:wa::Event::Launched{process:process(),started_at_ms:10}})?;
            let current=observer.worker_state().clone();
            let completed=complete(current,100).engineering.unwrap().completed.unwrap();
            observer.observe_worker_attempt(ProtocolEvent::WorkerAttemptObserved{request_id:request.id,observation:wa::Event::TurnCompleted{completion:completed}})?;
            observer.observe_worker_attempt(ProtocolEvent::WorkerAttemptObserved{request_id:request.id,observation:wa::Event::Sealed{candidate_sha256:id(10)}})?;
            Ok(WrapperResponse::Engineering(checked(observer.worker_state(),record())))
        }
    }
    let root=tempfile::tempdir().unwrap();let mut runtime=SupervisorRuntime::initialize_with_metadata(
        RuntimePaths::new(root.path()),initial(),trellis_kernel::runtime::RuntimeMetadata {
            repo_path:Some(root.path().to_path_buf()),..Default::default() }).unwrap();
    runtime.step(&mut ReadingAdapter).unwrap();let request=runtime.state().request_seq;
    assert!(runtime.step(&mut ReadingAdapter).unwrap_err().to_string().contains("owned checkpoint sink"));
    assert!(runtime.state().engineering.as_ref().unwrap().last_consumption.is_none());
    assert_eq!(runtime.state().in_flight_request.as_ref().unwrap().id,request);
    let restored=SupervisorRuntime::load(RuntimePaths::new(root.path())).unwrap();
    assert_eq!(restored.state(),runtime.state());
}

#[test] fn native_engineering_successor_contracts_preserve_checked_instructions_and_clear_stale_feedback() {
    let cases=[
        (implemented(planned(),20),Outcome::Review{decision:Decision::Revise,reason:"code-revision-marker".into()},Purpose::Implementation,"code-revision-marker"),
        (accept(submitted(accept(submitted(initial()),record())),record()),Outcome::Review{decision:Decision::Reject,reason:"plan-revision-marker".into()},Purpose::Plan,"plan-revision-marker"),
        (accept(exhausted(accept(exhausted(initial()),malformed())),malformed()),Outcome::Review{decision:Decision::Revise,reason:"retry-revision-marker".into()},Purpose::SourceReading,"retry-revision-marker"),
        (reviewed(planned(),20),Outcome::Improve{reason:"measured-cost-marker".into(),instruction:"implementation-improvement-marker".into()},Purpose::Implementation,"implementation-improvement-marker"),
    ];
    for (st,outcome,purpose,marker) in cases {
        let st=submitted(st);let ch=checked(&st,outcome);let origin=hash(&ch);
        let st=event(st,ProtocolEvent::EngineeringOutcome{checked:Box::new(ch)});let st=issue(st);
        let c=st.in_flight_request.as_ref().unwrap().engineering.as_ref().unwrap();assert_eq!(c.purpose,purpose);
        assert_eq!(c.assignment_feedback.as_ref().unwrap().checked_sha256,origin);
        assert!(serde_json::to_string(c).unwrap().contains(marker));
        if purpose==Purpose::SourceReading {
            assert!(c.assignment_feedback.as_ref().unwrap().prior_failure.is_some());
            let st=accept(seal(complete(launch(start(st)),100)),record());
            let st=issue(st);assert!(st.in_flight_request.as_ref().unwrap().engineering.as_ref().unwrap().assignment_feedback.is_none());
        }
    }
}
fn measured() -> (ProtocolState,String,String) {
    let st=reviewed(planned(),20);let eng=st.engineering.as_ref().unwrap();
    let content=hash(eng.candidate.as_ref().unwrap());let retained=eng.current_reviewed_candidate_sha256.clone().unwrap();
    let m=Measurement{candidate_sha256:content.clone(),reviewed_candidate_sha256:retained.clone(),harness_sha256:id(55),record_sha256:id(56),exit_codes:vec![0]};
    (accept(submitted(st),Outcome::Measure{reason:"original measurement".into(),measurement:m}),content,retained)
}
fn choose(st:ProtocolState,retained:String)->ProtocolState {
    accept(submitted(st),Outcome::Accept{reason:"earlier measured candidate remains best".into(),baseline:Baseline{
        reviewed_candidate_sha256:retained,harness_sha256:id(55),assessment_sha256:id(60),measurement_sha256:vec![id(56)],selection_receipt_sha256:id(61)}})
}

fn handoff_fixture() -> (ProtocolState, Handoff) {
    let (st, _, retained) = measured();
    let ready = choose(st, retained);
    let handoff = Handoff { predecessor_sha256: hash(&ready),
        baseline_sha256: hash(ready.engineering.as_ref().unwrap().baseline.as_ref().unwrap()),
        record_sha256: id(100), downstream_commit: "a".repeat(40),
        downstream_state_sha256: id(101), downstream_source_sha256: id(102) };
    (ready, handoff)
}

#[test] fn native_engineering_handoff_is_explicit_exact_and_idempotent() {
    let (ready, handoff) = handoff_fixture();
    let transferred = event(ready.clone(), ProtocolEvent::EngineeringHandoff { handoff: Box::new(handoff.clone()) });
    assert_eq!(transferred.stage, Stage::EngineeringReady);
    assert!(transferred.in_flight_request.is_none());
    assert!(!transferred.modeling_human_approved);
    assert_eq!(event(transferred.clone(), ProtocolEvent::EngineeringHandoff { handoff: Box::new(handoff.clone()) }), transferred);
    assert!(apply_event(transferred.clone(), ProtocolEvent::StartCycle).is_err());
    let mut wrong = handoff.clone(); wrong.downstream_commit = "b".repeat(40);
    assert!(apply_event(transferred, ProtocolEvent::EngineeringHandoff { handoff: Box::new(wrong) }).is_err());
    for field in 0..3 {
        let mut wrong = handoff.clone();
        match field { 0 => wrong.predecessor_sha256 = id(103), 1 => wrong.baseline_sha256 = id(104), _ => wrong.downstream_commit = "not-a-checkpoint".into() }
        assert!(apply_event(ready.clone(), ProtocolEvent::EngineeringHandoff { handoff: Box::new(wrong) }).is_err());
    }
    assert!(apply_event(initial(), ProtocolEvent::EngineeringHandoff { handoff: Box::new(handoff) }).is_err());
}

#[test] fn native_engineering_handoff_noop_sink_cannot_transfer_authority() {
    use trellis_kernel::{SupervisorRuntime, RuntimePaths, NoopCheckpointSink};
    let (ready, handoff) = handoff_fixture();
    let temp = tempfile::tempdir().unwrap();
    let mut runtime = SupervisorRuntime::initialize(RuntimePaths::new(temp.path()), ready.clone()).unwrap();
    assert!(runtime.step_injected_event_with_checkpoint_sink(
        ProtocolEvent::EngineeringHandoff { handoff: Box::new(handoff) }, &mut NoopCheckpointSink).is_err());
    assert_eq!(runtime.state(), &ready);
}
#[test] fn native_engineering_selected_candidate_restores_its_original_approved_plan_generation() {
    let (st,content,retained)=measured();let original=st.engineering.as_ref().unwrap().reviewed[&retained].clone();
    let st=accept(submitted(st),Outcome::Improve{reason:"dominant measured cost".into(),instruction:"evaluate a different plan".into()});
    assert_eq!(st.engineering.as_ref().unwrap().purpose,Purpose::Implementation);
    let st=accept(submitted(implemented(st,21)),review(Decision::Reject));
    assert_eq!(st.engineering.as_ref().unwrap().purpose,Purpose::Plan);assert!(st.engineering.as_ref().unwrap().current_reviewed_candidate_sha256.is_none());
    let mut p=plan();p.requirements[0].description="Revised representation proposal".into();
    let st=accept(submitted(st),Outcome::Record{summary:"revised plan".into(),record:p});
    let st=accept(submitted(st),review(Decision::Accept));let st=reviewed(st,21);let latest=st.request_seq;
    assert!(st.engineering.as_ref().unwrap().generation>original.generation);
    let st=choose(st,retained.clone());let eng=st.engineering.as_ref().unwrap();
    assert!(st.request_seq>latest);assert_eq!(st.stage,Stage::EngineeringReady);assert_eq!(eng.generation,original.generation);
    assert_eq!(eng.plan_sha256.as_ref(),Some(&original.candidate.plan_sha256));assert_eq!(eng.plan_approval_sha256.as_ref(),Some(&original.plan_approval_sha256));
    assert_eq!(hash(eng.candidate.as_ref().unwrap()),content);assert_eq!(eng.current_reviewed_candidate_sha256,Some(retained));st.validate().unwrap();
}
#[test] fn native_engineering_distinct_reviews_of_identical_content_preserve_both_measured_contexts() {
    let (st,content,retained)=measured();let old=st.engineering.as_ref().unwrap().reviewed[&retained].clone();
    let st=accept(submitted(st),Outcome::Improve{reason:"investigate".into(),instruction:"keep prior candidate if no gain".into()});
    let st=reviewed(st,20);
    assert_eq!(hash(st.engineering.as_ref().unwrap().candidate.as_ref().unwrap()),content);
    let eng=st.engineering.as_ref().unwrap();assert_eq!(eng.reviewed[&retained],old);assert_eq!(eng.reviewed.len(),2);
    assert_ne!(eng.current_reviewed_candidate_sha256.as_ref(),Some(&retained));
    let st=choose(st,retained);assert_eq!(st.stage,Stage::EngineeringReady);
}
#[test] fn native_engineering_retained_context_hash_and_current_pointer_are_checked() {
    let (st,_,retained)=measured();
    for n in 0..4 {
        let mut bad=st.clone();let eng=bad.engineering.as_mut().unwrap();
        match n {0=>eng.reviewed.get_mut(&retained).unwrap().review_sha256=id(90),1=>eng.reviewed.get_mut(&retained).unwrap().subject_sha256=id(91),2=>eng.current_reviewed_candidate_sha256=Some(id(92)),_=>eng.reviewed.get_mut(&retained).unwrap().generation+=1}
        assert!(bad.validate().is_err());
    }
}
#[test] fn native_engineering_exact_duplicate_launch_reuses_attempt_idempotence() {
    let st=launch(start(issue(initial())));
    let repeated=observed(st.clone(),wa::Event::Launched{process:process(),started_at_ms:10});assert_eq!(repeated,st);
    for start in [11,12] { assert!(apply_event(st.clone(),ProtocolEvent::WorkerAttemptObserved{request_id:st.request_seq,observation:wa::Event::Launched{process:process(),started_at_ms:start}}).is_err()); }
}

#[test] fn native_engineering_repo_checkpoint_reload_and_interrupted_publication_preserve_native_state() {
    use std::{path::{Path,PathBuf},fs,process::Command};
    use trellis_kernel::{SupervisorRuntime,RuntimePaths};
    use trellis_kernel::runtime::{RuntimeMetadata,CheckpointHookPayload,CheckpointSink};
    fn git(repo:&Path,args:&[&str]) {
        let result=Command::new("git").current_dir(repo).args(["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false"]).args(args).output().unwrap();
        assert!(result.status.success(),"synthetic Git fixture failed: {}",String::from_utf8_lossy(&result.stderr));
    }
    struct Sink {repo:PathBuf,interrupt:bool,guard:CheckpointGuard}
    impl CheckpointSink for Sink {
        fn engineering_guard(&self)->Option<&CheckpointGuard>{Some(&self.guard)}
        fn commit(&mut self,payload:&CheckpointHookPayload)->Result<(),String> {
            assert!(!payload.is_clean,"engineering checkpoint must not establish formal LastClean");
            let history=self.repo.join(".trellis-history");fs::create_dir_all(&history).unwrap();
            fs::write(history.join("supervisor_state.json"),serde_json::to_vec_pretty(&serde_json::json!({
                "event_count":payload.event_count,"metadata":payload.metadata,"checkpoint":payload.checkpoint,
                "state":payload.state,"commands":payload.commands})).unwrap()).unwrap();
            git(&self.repo,&["add","-A"]);git(&self.repo,&["commit","-qm","synthetic native checkpoint"]);
            if self.interrupt { panic!("synthetic interruption after checkpoint commit"); }
            Ok(())
        }
    }
    for interrupt in [false,true] {
        let root=tempfile::tempdir().unwrap();let repo=root.path().join("repo");fs::create_dir(&repo).unwrap();
        fs::write(repo.join("paper.txt"),"synthetic original input").unwrap();git(&repo,&["init","-q"]);git(&repo,&["add","-A"]);git(&repo,&["commit","-qm","synthetic input"]);
        let paths=RuntimePaths::new(root.path().join("runtime"));let st=submitted(initial());let ch=checked(&st,record());
        let mut runtime=SupervisorRuntime::initialize_with_metadata(paths.clone(),st,RuntimeMetadata{repo_path:Some(repo.clone()),..Default::default()}).unwrap();
        let guard=CheckpointGuard{request_id:ch.request_id,turn:ch.turn,request_sha256:ch.request_sha256.clone(),
            candidate_sha256:ch.candidate_sha256.clone(),checked_sha256:hash(&ch),roots:[
                DirectoryPin{path:"/fixture/view".into(),fd:17,device:1,inode:2},
                DirectoryPin{path:"/fixture/view/.trellis".into(),fd:18,device:1,inode:3}]};
        let mut sink=Sink{repo,interrupt,guard};
        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.step_injected_event_with_checkpoint_sink(ProtocolEvent::EngineeringOutcome{checked:Box::new(ch)},&mut sink)));
        if interrupt {assert!(result.is_err());} else {result.unwrap().unwrap();}
        let loaded=SupervisorRuntime::load(paths.clone()).unwrap();loaded.state().validate().unwrap();
        assert!(loaded.state().last_clean_commit.is_none());assert_eq!(loaded.state().engineering.as_ref().unwrap().purpose,Purpose::Plan);
        assert!(loaded.state().in_flight_request.is_none());assert!(!paths.root.join("checkpoint_transaction.pending.json").exists());
        let again=SupervisorRuntime::load(paths).unwrap();assert_eq!(again.state(),loaded.state());
    }
}

#[test] fn native_engineering_reopening_preserves_unreviewed_source_without_adopting_proposals() {
    for outcome in [review(Decision::Reject),Outcome::Propose{reason:"review a new departure before adoption".into()}] {
        let st=implemented(planned(),20);let source=st.engineering.as_ref().unwrap().candidate.clone();
        assert!(st.engineering.as_ref().unwrap().reviewed.is_empty());
        let st=accept(submitted(st),outcome);let eng=st.engineering.as_ref().unwrap();
        assert_eq!(eng.candidate,source);assert!(eng.current_reviewed_candidate_sha256.is_none());assert!(eng.plan_approval_sha256.is_none());
        let st=issue(st);let contract=st.in_flight_request.as_ref().unwrap().engineering.as_ref().unwrap();
        assert_eq!(contract.purpose,Purpose::Plan);assert_eq!(contract.candidate_sha256,source.as_ref().map(hash));
    }
    assert!(serde_json::from_value::<Outcome>(serde_json::json!({"outcome":"propose","reason":"proposal","candidate":{}})).is_err());
}
