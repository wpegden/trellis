//! Opt-in full-request corpus for human/independent prompt audits.
//! No provider dispatch, proof run, source edits, or fabricated production evidence.
use std::{collections::{BTreeMap, BTreeSet}, fs, path::Path};
use serde_json::{json, Value};
use trellis_kernel::{model::*, paper_implementation::{PaperImplementation, PaperImplementationRoute, PaperModelingChoice, PaperProofSupport, PaperProofLibrary, approval_basis}, request_contracts::populate_request_prompt_contracts};

fn nodes(names: &[&str]) -> BTreeSet<NodeId> { names.iter().map(|s| (*s).into()).collect() }
fn subject(route: PaperImplementationRoute) -> PaperImplementation {
    PaperImplementation { version:1, route, manifest_path:"paper-implementation/manifest.json".into(),
        subject_sha256:"a".repeat(64), configuration_sha256:"b".repeat(64), paper_path:"paper/main.tex".into(),
        paper_sha256:"c".repeat(64), implementation_roots:BTreeMap::from([("paper-result".into(),nodes(&["Implementation"]))]),
        modeling_choices: if route == PaperImplementationRoute::RustAeneas { vec![PaperModelingChoice {
            node:"ManualBinding".into(), source_locator:"crate/src/lib.rs:5; generated.lean:12".into(),
            rationale:"Model the diagnosed primitive with an exact preservation argument".into(),
            evidence_path:"modeling-input/diagnostic.json".into(), consequential:true, route_decision:None }] } else { vec![] },
        proof_support:None,
        excluded_complexity_clauses:BTreeMap::from([("paper-result".into(),vec!["The operation bound is outside v1; original paper line 7.".into()])]) }
}

fn base(mode: &str, phase: Phase) -> ProtocolState {
    let rust = mode.starts_with("rust") || mode == "pv";
    let paper = mode.starts_with("rust") || mode.starts_with("lean");
    let mut s = ProtocolState::default();
    s.phase=phase; s.cycle=3; s.stage=Stage::Reviewer;
    s.verifier_lanes=BTreeSet::from(["reviewer-a".into()]);
    s.pv_tablet_configured=rust;
    s.model_preservation_enabled=rust;
    s.pv_goal_mode=if rust { PvGoalMode::Prose } else { PvGoalMode::Spec };
    s.configured_targets=BTreeSet::from(["paper-result".into()]);
    s.live.present_nodes=nodes(&["Preamble","Implementation","ManualBinding","Refinement","Result"]);
    s.proof_nodes=nodes(&["Refinement","Result"]);
    s.live.open_nodes=s.proof_nodes.clone();
    s.node_kinds=BTreeMap::from([("Preamble".into(),NodeKind::Preamble),("Implementation".into(),NodeKind::Definition),
        ("ManualBinding".into(),NodeKind::Definition),("Refinement".into(),NodeKind::Proof),("Result".into(),NodeKind::Proof)]);
    s.deps=BTreeMap::from([("Result".into(),nodes(&["Refinement","Implementation"])),("Refinement".into(),nodes(&["Implementation","ManualBinding"]))]);
    s.target_claims=BTreeMap::from([("Result".into(),BTreeSet::from(["paper-result".into()]))]);
    s.live.coverage=BTreeMap::from([("paper-result".into(),nodes(&["Result"]))]);
    for node in &s.live.present_nodes {
        s.node_difficulty.insert(node.clone(),NodeDifficulty::Hard);
        s.easy_attempts.insert(node.clone(),0);
        s.live.target_fingerprints.insert(node.clone(),format!("statement:{node}"));
        s.live.corr_current_fingerprints.insert(node.clone(),format!("corr:{node}"));
        s.live.sound_current_fingerprints.insert(node.clone(),format!("sound:{node}"));
        s.live.substantiveness_current_fingerprints.insert(node.clone(),format!("substantive:{node}"));
    }
    s.live.paper_current_fingerprints.insert("paper-result".into(),"paper:fixture".into());
    s.active_node=Some("Result".into());
    if rust {
        s.node_role.insert("Implementation".into(),PvRole::ExtractionModel);
        s.configured_challenge_targets.insert("model:Implementation".into(),ChallengeTargetSpec {
            kind:ChallengeTargetKind::Def, name:"Implementation".into(),
            lean:"def Implementation (x : Nat) : Nat := x".into(), ..Default::default() });
        s.challenge_claims.insert("Implementation".into(),BTreeSet::from(["model:Implementation".into()]));
        s.live.challenge_coverage.insert("model:Implementation".into(),nodes(&["Implementation"]));
    }
    if mode.ends_with("required") { s.trust_base.mode=TrustBaseMode::RequiredV1; s.trust_base.format_version=TRUST_STATE_FORMAT_VERSION; }
    if paper {
        s.paper_implementation=Some(subject(if rust {PaperImplementationRoute::RustAeneas} else {PaperImplementationRoute::LeanNative}));
        if mode=="lean_mathlib" {
            s.paper_implementation.as_mut().unwrap().proof_support=Some(PaperProofSupport {kind:PaperProofLibrary::Mathlib,
                mathlib_rev:"d".repeat(40),dependency_inventory_sha256:"e".repeat(64),lake_manifest_sha256:"f".repeat(64)});
        }
        if matches!(phase,Phase::ProofFormalization|Phase::Cleanup) {
            s.paper_implementation_approved_sha256=Some(approval_basis(s.paper_implementation.as_ref().unwrap()));
            s.coarse_dag_nodes=s.live.present_nodes.clone();
        }
        if rust {
            s.modeling_route_decisions.push(json!({"request_id":17,"paper_implementation_basis":approval_basis(s.paper_implementation.as_ref().unwrap()),
                "paper_modeling_choices":["ManualBinding"],"resolution":{"action":"approve","scope":"Original representable inputs and return observations",
                    "limitations":"","declined_targets":[],"reopen_setup":false,"tasks":[{"id":"paper-model:ManualBinding","title":"Model the primitive","body":"Supply checked preservation and application arguments"}]},
                "assessment":"Fixture agent assessment; not an actual semantic approval."}));
        }
    }
    s.commit_live();
    s
}

fn emit(out:&Path,index:&mut Vec<Value>,name:String,mut state:ProtocolState,kind:RequestKind,scenario:&str,reachability:&str) {
    state.stage=match kind {RequestKind::Worker=>Stage::Worker,RequestKind::Review=>Stage::Reviewer,
        RequestKind::Paper=>Stage::VerifyPaper,RequestKind::Corr=>Stage::VerifyCorr,RequestKind::Sound=>Stage::VerifySound,
        RequestKind::Audit if state.phase==Phase::Modeling=>Stage::ModelingAudit,RequestKind::Audit=>Stage::CleanupAudit,
        RequestKind::StuckMathAudit=>Stage::StuckMathAudit,RequestKind::HumanGate=>Stage::HumanGate,
        RequestKind::Engineering=>panic!("Engineering prompts require a native engineering subject")};
    let validation=state.validate().err();
    let reachability = if state.trust_base.required() && !state.is_pv() {
        "REJECTED configuration: existing required_v1 profile supports PV only; no profile switching or authentic seed claimed"
    } else if state.trust_base.required() {
        "abstract required_v1 prompt carrier; omitted authenticated seed and launch binding prevent live admission"
    } else { reachability };
    let mut request=state.expected_request(42,kind);
    // Explicit scoped frontier packets avoid claiming that this synthetic
    // corpus has actually earned the verifier histories used by a scheduler.
    // Contract construction below is the production implementation unchanged.
    if kind==RequestKind::Paper {
        if scenario=="paper_target" { request.paper_verify_targets=BTreeSet::from(["paper-result".into()]);request.substantiveness_verify_nodes.clear(); }
        else {request.paper_verify_targets.clear();request.substantiveness_verify_nodes=nodes(&["Result","ManualBinding"]);}
    }
    if kind==RequestKind::Corr {request.verify_nodes=nodes(if scenario=="preamble_corr" {&["Preamble"]} else {&["Result","ManualBinding"]});request.corr_verify_nodes=request.verify_nodes.clone();}
    if kind==RequestKind::Sound {let n:NodeId=if scenario=="preamble_sound" {"Preamble"} else if scenario=="model_sound" {"ManualBinding"} else {"Result"}.into();request.sound_verify_node=Some(n.clone());request.verify_nodes=BTreeSet::from([n]);}
    populate_request_prompt_contracts(&mut request,None);
    fs::write(out.join(format!("{name}.state.json")),serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    fs::write(out.join(format!("{name}.request.json")),serde_json::to_vec_pretty(&request).unwrap()).unwrap();
    index.push(json!({"name":name,"kind":kind,"phase":state.phase,"scenario":scenario,"reachability":reachability,
        "request":format!("{name}.request.json"),"state":format!("{name}.state.json"),"state_validation_error":validation,
        "fixture_status":"Synthetic lane/frontier fixture using production expected_request and contract population. Fingerprints/review records are illustrative, not proofs or trusted production approvals.",
        "frontier_override":matches!(kind,RequestKind::Paper|RequestKind::Corr|RequestKind::Sound)}));
}

fn brief() -> GapBrief { GapBrief {node:"Result".into(),obligation:"The stated paper algorithm needs a representation bridge".into(),why_genuine:"Paper evidence leaves the representation choice unspecified".into(),localized_by_audit_request_id:21} }
fn audit_state(s:&mut ProtocolState) {s.stuck_math_audit.active=true;s.stuck_math_audit.trigger="Fixture request for an independent argument/algorithm audit".into();s.stuck_math_audit.active_since_cycle=s.cycle;}

#[test]
fn export_full_paper_mode_request_corpus() {
    let Ok(directory)=std::env::var("TRELLIS_EXPORT_PAPER_PROMPTS") else { return; };
    let out=Path::new(&directory); fs::create_dir_all(out).unwrap();
    let mut index=Vec::new();
    for mode in ["rust_seedless","rust_required","lean_seedless","lean_mathlib","lean_required","math","pv"] {
        for (phase,label) in [(Phase::Modeling,"modeling"),(Phase::TheoremStating,"stating"),(Phase::ProofFormalization,"proof"),(Phase::Cleanup,"cleanup")] {
            let reach=if (mode.starts_with("lean") || mode=="math") && phase==Phase::Modeling {"not_seeded_by_Lean_or_math_intake; diagnostic typed-kernel branch"}
                else {"supported_lane_shape; synthetic histories, not a run"};
            for (kind,scenario) in [(RequestKind::Worker,"worker"),(RequestKind::Review,"review"),(RequestKind::Paper,"paper_target"),
                (RequestKind::Paper,"paper_node"),(RequestKind::Corr,"corr"),(RequestKind::Corr,"preamble_corr"),
                (RequestKind::Sound,"sound"),(RequestKind::Sound,"model_sound"),(RequestKind::Sound,"preamble_sound")] {
                emit(out,&mut index,format!("{mode}-{label}-{scenario}"),base(mode,phase),kind,scenario,reach);
            }
            if matches!(phase,Phase::Modeling|Phase::Cleanup) {
                emit(out,&mut index,format!("{mode}-{label}-completion_audit"),base(mode,phase),RequestKind::Audit,"completion_audit",reach);
                let mut rejected=base(mode,phase); rejected.latest_audit_rejection_reason="Previous response omitted a required supporting artifact".into();
                emit(out,&mut index,format!("{mode}-{label}-completion_audit_retry"),rejected,RequestKind::Audit,"completion_audit_retry",reach);
            }
            if phase==Phase::Cleanup {continue;}
            for scenario in ["structural_audit","need_input_auditor","gap_planner","gap_critic","global_repair_audit","scoped_audit","initial_planner","coverage_planner","audit_retry"] {
                let mut s=base(mode,phase);audit_state(&mut s);
                match scenario {
                    "need_input_auditor"=>s.stuck_math_audit.need_input_audit=Some(NeedInputAuditContext {phase,active_node:s.active_node.clone(),reviewer_reason:"Resolve ambiguous paper hypotheses".into(),reviewer_comments:"Try autonomous refinement arguments first".into(),..Default::default()}),
                    "gap_planner"=>s.stuck_math_audit.gap_research=Some(GapResearchContext{gap_brief:brief(),..Default::default()}),
                    "gap_critic"=>s.stuck_math_audit.gap_plan_critique=Some(GapPlanCritiqueContext{gap_brief:brief(),route_tex:"Prove the implementation refinement before applying the paper theorem.".into(),..Default::default()}),
                    "global_repair_audit"=>s.pending_global_repair_request=Some(PendingGlobalRepairRequest{proposed_extension_nodes:nodes(&["ManualBinding"]),reviewer_reason:"Repair a shared representation lemma".into(),..Default::default()}),
                    "scoped_audit"=>s.scoped_audit=Some(trellis_kernel::coarse_audits::AuditContext{retry:0,feedback:String::new(),memory_entry_ids:BTreeSet::new(),goals:nodes(&["Result"]),nodes:s.live.present_nodes.clone(),reason:"Assess the Result refinement cone".into(),previous:BTreeMap::new(),completed_attempts:BTreeMap::new(),recent_attempts:Vec::new()}),
                    "initial_planner"|"coverage_planner"=>s.stuck_math_audit.initial_planning=Some(InitialPlanningContext{source:if mode=="pv" {InitialPlanningSource::PvGoalProse{goal_path:"GOAL.md".into()}} else {InitialPlanningSource::PaperManuscript{paper_tex_path:"paper/main.tex".into()}},configured_target_ids:BTreeSet::from(["paper-result".into()]),coverage_replanning:scenario=="coverage_planner",uncovered_target_ids:if scenario=="coverage_planner" {BTreeSet::from(["paper-result".into()])} else {BTreeSet::new()},..Default::default()}),
                    "audit_retry"=>{s.latest_stuck_math_audit_rejection_reason="Missing required route comparison".into();s.stuck_math_audit_burst_retry_count=1;}, _=>{}
                }
                let special=if phase==Phase::Modeling && matches!(scenario,"need_input_auditor"|"gap_planner"|"gap_critic") {"not_ordinary_Modeling_dispatch; native modeling routes emitted separately"}
                    else if phase!=Phase::TheoremStating && matches!(scenario,"initial_planner"|"coverage_planner") {"not_a_fresh_stating_planner; defensive contract probe"} else {reach};
                emit(out,&mut index,format!("{mode}-{label}-{scenario}"),s,RequestKind::StuckMathAudit,scenario,special);
            }
            for role in [RequestKind::Worker,RequestKind::Review] {
                let mut s=base(mode,phase);
                s.audit_plan=Some(AuditPlan{report:"Audit recommends proving the refinement explicitly".into(),tasks:vec![AuditTask{id:"bridge".into(),title:"Complete refinement".into(),body:"Prove the result for the actual implementation".into(),..Default::default()}],written_at_cycle:2,written_by_request:25,..Default::default()});
                emit(out,&mut index,format!("{mode}-{label}-{role:?}_plan"),s,role,"audit_plan_continuation",reach);
            }
        }
        for step in 0..5 {
            let mut s=base(mode,Phase::Modeling);audit_state(&mut s);
            s.modeling_request=Some(ModelingRequest::ReviewRoute{reason:"Exact manual primitive modeling needs approval".into()});
            let mut review=ModelingRouteReview{targets:s.configured_targets.clone(),basis_fingerprint:"illustrative-modeling-basis".into(),..Default::default()};
            for round in 0..step/2 {review.submit_plan(format!("Round {round}: paper-model:ManualBinding; compare exact semantics with narrowing scope")).unwrap();review.submit_critique("accept".into(),"Independent assessment of scope and preservation evidence".into(),String::new()).unwrap();}
            if step%2==1 {review.submit_plan("paper-model:ManualBinding; exact semantics and support obligations".into()).unwrap();s.stuck_math_audit.gap_plan_critique=Some(GapPlanCritiqueContext{gap_brief:brief(),route_tex:review.pending_route.clone().unwrap(),..Default::default()});}
            else if step<4 {s.stuck_math_audit.gap_research=Some(GapResearchContext{gap_brief:brief(),..Default::default()});}
            s.modeling_route_review=Some(review);
            let scenario=["route_planner1","route_critic1","route_planner2","route_critic2","route_final_audit"][step];
            emit(out,&mut index,format!("{mode}-modeling-{scenario}"),s,if step==4 {RequestKind::Audit} else {RequestKind::StuckMathAudit},scenario,
                if mode.starts_with("lean")||mode=="math" {"not_seeded_on_this_route; native_Rust_Modeling_only"} else {"production two-round modeling-route carrier; agent records are test fixtures"});
        }
        // These are explicitly rejected paper states, retained to ensure audit
        // readers do not confuse a renderer's tolerance with runtime authority.
        let mut s=base(mode,Phase::TheoremStating);audit_state(&mut s);
        s.stuck_math_audit.assumptions_lane=Some(AssumptionsLaneContext{assumption_id:"unsupported".into(),candidate_invariant:"Unproved source premise".into(),axiom_name:"unsupported".into(),..Default::default()});
        emit(out,&mut index,format!("{mode}-rejected-assumptions_lane"),s,RequestKind::StuckMathAudit,"assumptions_lane",
            if mode=="pv" {"ordinary_PV_control_only"} else {"REJECTED paper state; never an authorized v1 assumptions lane"});
    }
    let names: BTreeSet<_> = index.iter().map(|case| case["name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), index.len(), "prompt cases must have distinct output filenames");
    fs::write(out.join("index.json"),serde_json::to_vec_pretty(&json!({"schema":"trellis-full-prompt-audit/v1","producer":"ProtocolState::expected_request + populate_request_prompt_contracts","provider_dispatch":false,"cases":index})).unwrap()).unwrap();
    eprintln!("exported {} full request/state pairs to {}",index.len(),out.display());
}

#[test]
fn scoped_paper_request_preserves_authority_without_widening_node_scope() {
    for mode in ["rust_seedless", "rust_required", "lean_seedless", "lean_mathlib", "lean_required"] {
        let mut state=base(mode,Phase::ProofFormalization);
        state.scoped_audit=Some(trellis_kernel::coarse_audits::AuditContext {
            retry:0,feedback:String::new(),memory_entry_ids:BTreeSet::new(),goals:nodes(&["Result"]),
            nodes:nodes(&["Result","Implementation"]),reason:"Independent scoped audit".into(),
            previous:BTreeMap::new(),completed_attempts:BTreeMap::new(),recent_attempts:Vec::new() });
        let request=state.expected_request(19,RequestKind::StuckMathAudit);
        assert_eq!(request.paper_implementation,state.paper_implementation);
        assert_eq!(request.current_present_nodes,nodes(&["Result","Implementation"]));
        assert!(!request.current_node_kinds.contains_key("ManualBinding"));
        assert!(request.stuck_math_audit_contract["prompt_fragments"].as_array().unwrap()
            .contains(&json!("paper_implementation/authority.md")));
        assert!(request.stuck_math_audit_contract["artifact_contract"]["prompt_schema_example"]
            .get("under_model_candidate_invariant").is_none());
        assert_eq!(request.paper_implementation_review["subject"],json!(state.paper_implementation.unwrap()));
    }
}


#[test]
fn ordinary_scoped_audit_projection_remains_without_paper_route_context() {
    let mut state=base("pv",Phase::ProofFormalization);
    state.modeling_route_decisions.push(json!({"request_id":17,"resolution":{"action":"approve"}}));
    state.scoped_audit=Some(trellis_kernel::coarse_audits::AuditContext {
        retry:0,feedback:String::new(),memory_entry_ids:BTreeSet::new(),goals:nodes(&["Result"]),
        nodes:nodes(&["Result"]),reason:"Ordinary scoped audit".into(),previous:BTreeMap::new(),
        completed_attempts:BTreeMap::new(),recent_attempts:Vec::new() });
    let request=state.expected_request(19,RequestKind::StuckMathAudit);
    assert!(request.paper_implementation.is_none());
    assert!(request.paper_implementation_review.is_null());
    assert!(request.modeling_route_decisions.is_empty());
}
