------------------------------- MODULE SupervisorCoreSim_bugcheck -------------------------------
EXTENDS SupervisorCoreSim

(***************************************************************************)
(* Sanity check: this module exposes a deliberately corrupted Init that   *)
(* fixes `gateKind = "advance"` while `stage = "Start"`.  The              *)
(* `HumanGateMatchesState` invariant says `gateKind # "none" <=> stage =  *)
(* "HumanGate"`; the deliberately-corrupt Init violates the biconditional *)
(* immediately, so TLC should report a violation on State 1.              *)
(*                                                                       *)
(* Used to verify TLC's invariant-checking machinery is wired correctly. *)
(***************************************************************************)

DeliberatelyBuggyInit ==
    /\ phase = "theorem_stating"
    /\ stage = "Start"
    /\ cycle = 0
    /\ activeNode = "NONE"
    /\ activeCoarseNode = "NONE"
    /\ presentNodes = SimInitialPresentNodes
    /\ openNodes = {}
    /\ coverage = [t \in SimTargets |-> {}]
    /\ approvedCoverage = [t \in SimTargets |-> {}]
    /\ configuredTargets = SimInitialConfiguredTargets
    /\ approvedConfiguredTargets = {}
    /\ coarseDagNodes = {}
    /\ correspondenceStatus  = [n \in SimNodes |-> "unknown"]
    /\ substantivenessStatus = [n \in SimNodes |-> "unknown"]
    /\ soundnessStatus       = [n \in SimNodes |-> "unknown"]
    /\ deviationStatus       = [n \in SimNodes |-> "pass"]
    /\ faithfulnessStatus    = [t \in SimTargets |-> "unknown"]
    /\ localClosureStatus    = [n \in SimNodes |-> "verified"]
    /\ authorizedNodes = {}
    /\ gateKind = "advance"   \* DELIBERATE BUG: stage="Start" but gateKind != "none"
    /\ humanInputOutstanding = FALSE
    /\ pendingProtectedReapproval = {}
    /\ hasPendingTask = FALSE
    /\ pendingTaskKind = "none"
    /\ pendingTaskCarriers = {}
    /\ workerMode = "local"
    /\ cleanupAuditActive = FALSE
    /\ stuckMathAuditActive = FALSE
    /\ needInputAuditorActive = FALSE
    /\ postAdvanceRoutingPending = FALSE
    /\ forceReviewAfterConeClean = FALSE
    /\ globalRepairStep = "none"
    /\ cyclesSinceClean = 0
    /\ hasEverBeenClean = FALSE
    /\ inFlightRequestKind = "none"

DeliberatelyBuggySpec == DeliberatelyBuggyInit /\ [][Core!Next]_Core!Vars

=============================================================================
