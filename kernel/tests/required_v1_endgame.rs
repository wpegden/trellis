//! Reduced required-v1 persistence and completion boundary.

use trellis_kernel::{
    ChallengeResolution, ChallengeTargetId, ChallengeTargetKind, ChallengeTargetSpec,
    ProtocolState, TrustBaseMode,
};

#[test]
fn new_trust_state_round_trip_has_no_retired_authority() {
    let mut state = ProtocolState::default();
    state.trust_base.mode = TrustBaseMode::RequiredV1;
    state.trust_base.format_version = trellis_kernel::model::TRUST_STATE_FORMAT_VERSION;
    let bytes = serde_json::to_vec(&state).unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    for retired in [
        "disproof_station_records",
        "source_assessment",
        "give_up",
        "witness_evaluation",
    ] {
        assert!(!text.contains(retired), "new state contains {retired}");
    }
    let restored: ProtocolState = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(restored.trust_base.format_version, 2);
}

#[test]
fn old_authorization_bearing_fixture_is_refused() {
    let bytes = include_bytes!("fixtures/state_fixture_roundtrip/pre_prune_trust_state.json");
    assert!(serde_json::from_slice::<ProtocolState>(bytes).is_err());
}

#[test]
fn open_target_is_non_completion() {
    let target = ChallengeTargetId::from("goal:open");
    let mut state = ProtocolState::default();
    state.configured_challenge_targets.insert(
        target.clone(),
        ChallengeTargetSpec {
            kind: ChallengeTargetKind::Theorem,
            name: "OpenTarget".into(),
            lean: "theorem OpenTarget : True := by".into(),
            resolution: ChallengeResolution::Prove,
            ..ChallengeTargetSpec::default()
        },
    );
    state.live.open_nodes.insert("OpenTarget".into());
    let error = trellis_kernel::trust_base::terminal_outcome(&state, &target).unwrap_err();
    assert!(error.contains("no checked successful result"), "{error}");
}
