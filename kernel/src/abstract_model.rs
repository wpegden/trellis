use crate::engine::{apply_event, ProtocolCommand, ProtocolEvent, TransitionError};
use crate::model::ProtocolState;
use serde::{Deserialize, Serialize};

pub type AbstractState = ProtocolState;
pub type AbstractCommand = ProtocolCommand;
pub type AbstractEvent = ProtocolEvent;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecAction {
    StartCycle,
    AcceptValidWorker,
    AcceptInvalidWorker,
    AcceptStuckWorker,
    AcceptPaperArtifact,
    AcceptCorrArtifact,
    AcceptSoundArtifact,
    ReviewContinueAfterInvalid,
    ReviewNeedInputAfterInvalid,
    ReviewContinueAfterValid,
    ReviewNeedInputAfterValid,
    ReviewAdvancePhase,
    ReviewContinueProof,
    ReviewNeedInputProof,
    ReviewContinueCleanup,
    ReviewNeedInputCleanup,
    ReviewDoneCleanup,
    HumanApproveAdvance,
    HumanFeedbackAfterAdvance,
    HumanResolveNeedInput,
    /// StuckMathAudit response that confirms the reviewer's NeedInput
    /// escalation: routes to HumanGate (NeedInput) with
    /// `gate_from_invalid_attempt` taken from the latched
    /// `NeedInputAuditContext`. Mirror of kernel
    /// `apply_stuck_math_audit_response` Valid + need_input_audit Some +
    /// `confirm_need_input = true` arm (engine.rs).
    AcceptStuckMathAuditDispatchHumanGate,
    /// StuckMathAudit response that decides the reviewer's escalation
    /// was unnecessary, or a plain audit-done response with no
    /// need-input context: routes back to Reviewer. Mirror of
    /// `apply_stuck_math_audit_response`'s back-to-reviewer arms.
    AcceptStuckMathAuditBackToReviewer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceStep {
    pub action: SpecAction,
    pub event: AbstractEvent,
    pub expected: AbstractState,
    pub expected_commands: Vec<AbstractCommand>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceCase {
    pub name: String,
    pub initial: AbstractState,
    pub steps: Vec<TraceStep>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceFixture {
    pub cases: Vec<TraceCase>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbstractTransitionOutcome {
    pub state: AbstractState,
    pub commands: Vec<AbstractCommand>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionRequest {
    pub state: AbstractState,
    pub event: AbstractEvent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionErrorPayload {
    pub kind: String,
    pub message: String,
}

impl From<TransitionError> for TransitionErrorPayload {
    fn from(error: TransitionError) -> Self {
        match error {
            TransitionError::InvalidStage { expected, found } => Self {
                kind: "invalid_stage".into(),
                message: format!("expected stage {expected}, found {:?}", found),
            },
            TransitionError::InvalidPhase { expected, found } => Self {
                kind: "invalid_phase".into(),
                message: format!("expected phase {:?}, found {:?}", expected, found),
            },
            TransitionError::CycleMismatch { expected, found } => Self {
                kind: "cycle_mismatch".into(),
                message: format!("expected cycle {expected}, found {found}"),
            },
            TransitionError::RequestMismatch {
                expected,
                found_kind,
                found_request_id,
                found_cycle,
            } => Self {
                kind: "request_mismatch".into(),
                message: format!(
                    "expected request {:?}, found kind={:?} id={} cycle={}",
                    expected, found_kind, found_request_id, found_cycle
                ),
            },
            TransitionError::IllegalReviewerDecision => Self {
                kind: "illegal_reviewer_decision".into(),
                message: "reviewer decision violates protocol legality checks".into(),
            },
            TransitionError::IllegalResponse(message) => Self {
                kind: "illegal_response".into(),
                message,
            },
            TransitionError::InvariantViolation(message) => Self {
                kind: "invariant_violation".into(),
                message,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TransitionResponse {
    Success {
        state: AbstractState,
        commands: Vec<AbstractCommand>,
    },
    Error {
        error: TransitionErrorPayload,
    },
}

pub fn apply_abstract_event(
    state: AbstractState,
    event: AbstractEvent,
) -> Result<AbstractTransitionOutcome, TransitionError> {
    let outcome = apply_event(state, event)?;
    Ok(AbstractTransitionOutcome {
        state: outcome.state,
        commands: outcome.commands,
    })
}

pub fn apply_transition_request(request: TransitionRequest) -> TransitionResponse {
    match apply_abstract_event(request.state, request.event) {
        Ok(outcome) => TransitionResponse::Success {
            state: outcome.state,
            commands: outcome.commands,
        },
        Err(error) => TransitionResponse::Error {
            error: error.into(),
        },
    }
}
