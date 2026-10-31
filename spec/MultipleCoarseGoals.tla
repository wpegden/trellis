-------------------------- MODULE MultipleCoarseGoals --------------------------
EXTENDS Naturals, FiniteSets, TLC

\* Scheduler projection. Mathematical acceptance and global/human approval
\* remain the existing SupervisorProtocol transitions. Goal completion below
\* means both shallow closure and relevant verification have actually finished.
CONSTANTS Goals, Nodes, GoalSlots, WorkerSlots, Region
VARIABLES active, completed, eliminated, committed, workers, queued, audits,
          version, frozen, reports
vars == <<active, completed, eliminated, committed, workers, queued, audits,
          version, frozen, reports>>
Evidence(gs) == UNION {Region[g] : g \in gs}

Init == /\ active = {} /\ completed = {} /\ eliminated = {} /\ committed = {}
        /\ workers = {} /\ queued = {} /\ audits = {}
        /\ version = [n \in Nodes |-> 0]
        /\ frozen = [g \in Goals |-> [n \in Nodes |-> 0]]
        /\ reports = [g \in Goals |-> 0]

Select(g) ==
    /\ g \notin committed /\ Cardinality(active) < GoalSlots
    /\ active' = active \cup {g} /\ committed' = committed \cup {g}
    /\ UNCHANGED <<completed, eliminated, workers, queued, audits, version, frozen, reports>>

\* A closure attempt needs no goal place. Both ordinary and closure writes
\* wait for any queued audit that needs their source, preventing starvation.
Work(n) ==
    /\ n \notin workers /\ Cardinality(workers) < WorkerSlots
    /\ n \notin Evidence(queued \cup audits)
    /\ workers' = workers \cup {n}
    /\ UNCHANGED <<active, completed, eliminated, committed, queued, audits, version, frozen, reports>>
FinishWork(n) ==
    /\ n \in workers /\ workers' = workers \ {n}
    /\ version' = [version EXCEPT ![n] = 1 - @]
    /\ UNCHANGED <<active, completed, eliminated, committed, queued, audits, frozen, reports>>

Complete(g) ==
    /\ g \in active /\ workers \cap Region[g] = {}
    /\ active' = active \ {g} /\ completed' = completed \cup {g}
    /\ UNCHANGED <<eliminated, committed, workers, queued, audits, version, frozen, reports>>
Regress(g) ==
    /\ g \in completed /\ completed' = completed \ {g}
    /\ active' = active \cup {g}
    \* A restored obligation may exceed admission capacity; it evicts none.
    /\ UNCHANGED <<eliminated, committed, workers, queued, audits, version, frozen, reports>>
Eliminate(g) ==
    \* Abstracts an already-authorized structural deletion of an obsolete root.
    /\ g \in active /\ workers = {} /\ audits = {} /\ queued = {}
    /\ active' = active \ {g} /\ eliminated' = eliminated \cup {g}
    /\ UNCHANGED <<completed, committed, workers, queued, audits, version, frozen, reports>>
RequestAudit(g) ==
    /\ g \in active /\ g \notin queued \cup audits
    /\ queued' = queued \cup {g}
    /\ UNCHANGED <<active, completed, eliminated, committed, workers, audits, version, frozen, reports>>
StartAudit(g) ==
    /\ g \in queued /\ workers \cap Region[g] = {}
    /\ queued' = queued \ {g} /\ audits' = audits \cup {g}
    /\ frozen' = [frozen EXCEPT ![g] = version]
    /\ UNCHANGED <<active, completed, eliminated, committed, workers, version, reports>>
FinishAudit(g) ==
    /\ g \in audits /\ \A n \in Region[g] : version[n] = frozen[g][n]
    /\ audits' = audits \ {g} /\ reports' = [reports EXCEPT ![g] = 1 - @]
    \* Report publication cannot complete, eliminate, or replace a commitment.
    /\ UNCHANGED <<active, completed, eliminated, committed, workers, queued, version, frozen>>
Reset == UNCHANGED vars
Next == Reset \/ (\E g \in Goals : Select(g) \/ Complete(g) \/ Regress(g)
    \/ Eliminate(g) \/ RequestAudit(g) \/ StartAudit(g) \/ FinishAudit(g))
    \/ (\E n \in Nodes : Work(n) \/ FinishWork(n))
Spec == Init /\ [][Next]_vars

NoParking == active = committed \ (completed \cup eliminated)
NoResurrection == active \cap eliminated = {}
WorkerCapacity == Cardinality(workers) <= WorkerSlots
AuditOwnership == \A g \in audits : workers \cap Region[g] = {}
AuditFreshness == \A g \in audits : \A n \in Region[g] : version[n] = frozen[g][n]
SmokeRegion == [g \in {"A", "B", "C"} |-> {g, "shared"}]
=============================================================================
