------------------------------- MODULE SupervisorCoreSim_isolation_bugcheck -------------------------------
EXTENDS SupervisorCoreSim_isolation

(***************************************************************************)
(* Phase IV step 12 — teeth check (THROWAWAY, expected-FAIL, NOT in the    *)
(* green suite).                                                          *)
(*                                                                       *)
(* Same 2-node / 2-backend / `n1 -> n2` edge setup as                    *)
(* SupervisorCoreSim_isolation, but a DELIBERATELY NON-UNIFORM            *)
(* `nodeTarget` (`n1 :> "lean"`, `n2 :> "isabelle_hol"`).  The formal     *)
(* import `n1 -> n2` then crosses backend targets, so                    *)
(* `CrossTargetFormalIsolation` is VIOLATED on State 1.  This proves the  *)
(* invariant (and, by the byte-identical kernel mirror, the kernel        *)
(* `validate()` check) actually has teeth — it would catch a real         *)
(* cross-target import.  Run once to confirm the violation; never add to  *)
(* the green suite.                                                       *)
(***************************************************************************)

CrossTargetBuggyInit ==
    /\ phase = "theorem_stating"
    /\ stage = "Start"
    /\ cycle = 0
    /\ activeNode = "NONE"
    /\ activeCoarseNode = "NONE"
    /\ presentNodes = SimInitialPresentNodes
    /\ openNodes = {}
    /\ coverage = [t \in SimTargets |-> {}]
    /\ approvedCoverage = [t \in SimTargets |-> {}]
    /\ challengeCoverage = [t \in SimChallengeTargets |-> {}]
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
    /\ gateKind = "none"
    /\ humanInputOutstanding = FALSE
    /\ pendingProtectedReapproval = {}
    /\ hasPendingTask = FALSE
    /\ pendingTaskKind = "none"
    /\ pendingTaskCarriers = {}
    /\ workerMode = "local"
    /\ cleanupAuditActive = FALSE
    /\ stuckMathAuditActive = FALSE
    /\ needInputAuditorActive = FALSE
    /\ gapPlannerActive = FALSE
    /\ gapCriticActive = FALSE
    /\ gapRejectCount = 0
    /\ postAdvanceRoutingPending = FALSE
    /\ forceReviewAfterConeClean = FALSE
    /\ globalRepairStep = "none"
    /\ cyclesSinceClean = 0
    /\ hasEverBeenClean = FALSE
    /\ inFlightRequestKind = "none"
    \* DELIBERATE BUG: a formal import n1 -> n2 across two backends
    \* (n1 :> "lean", n2 :> "isabelle_hol").
    /\ nodeTarget = [n \in SimNodes |-> IF n = "n1" THEN "lean" ELSE "isabelle_hol"]
    /\ challengePolarity = [t \in SimChallengeTargets |-> "prove"]
    /\ polarityFlips = [t \in SimChallengeTargets |-> 0]
    /\ challengeClosedSide = [t \in SimChallengeTargets |-> "open"]
    /\ sidecarCloses = 0
    /\ sidecarQueue = {}

CrossTargetBuggySpec == CrossTargetBuggyInit /\ [][Core!Next]_Core!Vars

=============================================================================
