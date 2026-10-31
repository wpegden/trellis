----------------------- MODULE Phase0IncompleteHarness -----------------------
EXTENDS Integers

CONSTANT MaxPhase0Handbacks
ASSUME MaxPhase0Handbacks \in Nat \ {0}

VARIABLES stage, generation, handbacks, initialDiagnosticReturned,
          checkerFailureSeen

vars == <<stage, generation, handbacks, initialDiagnosticReturned,
          checkerFailureSeen>>

Init ==
    /\ stage = "Check"
    /\ generation = 0
    /\ handbacks = 0
    /\ initialDiagnosticReturned = FALSE
    /\ checkerFailureSeen = FALSE

Phase0InitialCheckerFailure ==
    /\ stage = "Check"
    /\ ~initialDiagnosticReturned
    /\ stage' = "Worker"
    /\ initialDiagnosticReturned' = TRUE
    /\ UNCHANGED <<generation, handbacks, checkerFailureSeen>>

Phase0WorkerRevision ==
    /\ stage = "Worker"
    /\ stage' = "Check"
    /\ generation' = generation + 1
    /\ UNCHANGED <<handbacks, initialDiagnosticReturned, checkerFailureSeen>>

Phase0CheckerFailure ==
    /\ stage = "Check"
    /\ initialDiagnosticReturned
    /\ ~checkerFailureSeen
    /\ stage' = "Worker"
    /\ checkerFailureSeen' = TRUE
    /\ UNCHANGED <<generation, handbacks, initialDiagnosticReturned>>

Phase0CheckSuccess ==
    /\ stage = "Check"
    /\ initialDiagnosticReturned
    /\ checkerFailureSeen
    /\ stage' = "SeamAudit"
    /\ UNCHANGED <<generation, handbacks, initialDiagnosticReturned,
                    checkerFailureSeen>>

Phase0SeamAuditPass ==
    /\ stage = "SeamAudit"
    /\ stage' = "CorrespondenceAudit"
    /\ UNCHANGED <<generation, handbacks, initialDiagnosticReturned,
                    checkerFailureSeen>>

Phase0AuditFindings ==
    /\ stage = "CorrespondenceAudit"
    /\ handbacks < MaxPhase0Handbacks
    /\ handbacks' = handbacks + 1
    /\ stage' = IF handbacks' = MaxPhase0Handbacks
                    THEN "Incomplete" ELSE "Worker"
    /\ UNCHANGED <<generation, initialDiagnosticReturned, checkerFailureSeen>>

Phase0IncompleteStop ==
    /\ stage = "Incomplete"
    /\ UNCHANGED vars

Next ==
    \/ Phase0InitialCheckerFailure
    \/ Phase0WorkerRevision
    \/ Phase0CheckerFailure
    \/ Phase0CheckSuccess
    \/ Phase0SeamAuditPass
    \/ Phase0AuditFindings
    \/ Phase0IncompleteStop

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ stage \in {"Worker", "Check", "SeamAudit", "CorrespondenceAudit", "Incomplete"}
    /\ generation \in Nat
    /\ handbacks \in 0..MaxPhase0Handbacks
    /\ initialDiagnosticReturned \in BOOLEAN
    /\ checkerFailureSeen \in BOOLEAN

=============================================================================
