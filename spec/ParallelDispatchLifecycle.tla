------------------------ MODULE ParallelDispatchLifecycle ------------------------
EXTENDS Naturals, FiniteSets, TLC

\* Bounded lifecycle smoke. Mathematical acceptance is an external predicate;
\* this model does not replace SupervisorProtocol's approval/phase gates.
\* RequestsArchived changes only exact request storage representation. It is
\* a stuttering step on these lifecycle variables; the kernel refuses it at
\* completed/PV-frozen boundaries. Object integrity and recovery are covered
\* by native filesystem/Git regressions, not mathematical approval actions.
CONSTANTS Tasks, Conflicts, WorkerSlots, CompilerSlots
SmokeConflicts == {<<"a", "c">>, <<"c", "a">>}
VARIABLES status, agents, compilers, expired, fresh, journal, files, accepted,
          phaseAdvanced, crashed
vars == <<status, agents, compilers, expired, fresh, journal, files, accepted,
          phaseAdvanced, crashed>>
Occupied == {t \in Tasks : status[t] \notin {"idle", "resolved"}}

Init == /\ status = [t \in Tasks |-> "idle"]
        /\ agents = {} /\ compilers = {} /\ expired = {} /\ fresh = Tasks
        /\ journal = {} /\ files = {} /\ accepted = {}
        /\ phaseAdvanced = FALSE /\ crashed = FALSE

Reserve(t) ==
    /\ ~crashed /\ ~phaseAdvanced /\ status[t] = "idle"
    /\ Cardinality(Occupied) < WorkerSlots
    /\ \A other \in Occupied : <<t, other>> \notin Conflicts
    /\ status' = [status EXCEPT ![t] = "reserved"]
    /\ UNCHANGED <<agents, compilers, expired, fresh, journal, files, accepted, phaseAdvanced, crashed>>

Launch(t) ==
    /\ ~crashed /\ status[t] = "reserved"
    /\ status' = [status EXCEPT ![t] = "running"] /\ agents' = agents \cup {t}
    /\ UNCHANGED <<compilers, expired, fresh, journal, files, accepted, phaseAdvanced, crashed>>

Check(t) ==
    /\ ~crashed /\ status[t] \in {"running", "checking"}
    /\ t \notin compilers /\ t \notin expired /\ journal = {}
    /\ Cardinality(compilers) < CompilerSlots
    /\ compilers' = compilers \cup {t}
    /\ UNCHANGED <<status, agents, expired, fresh, journal, files, accepted, phaseAdvanced, crashed>>

ReapCompiler(t) ==
    /\ t \in compilers /\ compilers' = compilers \ {t}
    /\ UNCHANGED <<status, agents, expired, fresh, journal, files, accepted, phaseAdvanced, crashed>>

Finish(t) ==
    /\ ~crashed /\ status[t] = "running"
    /\ status' = [status EXCEPT ![t] = "checking"] /\ agents' = agents \ {t}
    /\ UNCHANGED <<compilers, expired, fresh, journal, files, accepted, phaseAdvanced, crashed>>

Expire(t) ==
    /\ ~crashed /\ status[t] = "running" /\ journal # {t}
    /\ status' = [status EXCEPT ![t] = "stopping"] /\ expired' = expired \cup {t}
    /\ UNCHANGED <<agents, compilers, fresh, journal, files, accepted, phaseAdvanced, crashed>>

Stale(t) ==
    /\ ~crashed /\ t \in fresh /\ journal = {} /\ t \notin accepted
    /\ fresh' = fresh \ {t}
    /\ UNCHANGED <<status, agents, compilers, expired, journal, files, accepted, phaseAdvanced, crashed>>

Reject(t) ==
    /\ ~crashed /\ status[t] = "checking" /\ journal # {t}
    /\ status' = [status EXCEPT ![t] = "stopping"]
    /\ UNCHANGED <<agents, compilers, expired, fresh, journal, files, accepted, phaseAdvanced, crashed>>

Prepare(t) ==
    /\ ~crashed /\ status[t] = "checking" /\ t \notin compilers
    /\ t \notin expired /\ t \in fresh /\ journal = {}
    \* A sealed candidate has no worker deadline. The durable decision precedes
    \* canonical writes; restart must finish it without changing its outcome.
    /\ journal' = {t}
    /\ UNCHANGED <<status, agents, compilers, expired, fresh, files, accepted, phaseAdvanced, crashed>>

PublishFiles(t) ==
    /\ ~crashed /\ journal = {t} /\ t \notin files
    /\ files' = files \cup {t}
    /\ UNCHANGED <<status, agents, compilers, expired, fresh, journal, accepted, phaseAdvanced, crashed>>

PublishState(t) ==
    /\ ~crashed /\ journal = {t} /\ t \in files /\ t \notin accepted
    /\ accepted' = accepted \cup {t} /\ status' = [status EXCEPT ![t] = "stopping"]
    /\ UNCHANGED <<agents, compilers, expired, fresh, journal, files, phaseAdvanced, crashed>>

Receipt(t) ==
    /\ ~crashed /\ journal = {t} /\ t \in accepted /\ journal' = {}
    /\ UNCHANGED <<status, agents, compilers, expired, fresh, files, accepted, phaseAdvanced, crashed>>

ReapAgent(t) ==
    /\ status[t] = "stopping" /\ t \in agents /\ agents' = agents \ {t}
    /\ UNCHANGED <<status, compilers, expired, fresh, journal, files, accepted, phaseAdvanced, crashed>>

Resolve(t) ==
    /\ ~crashed /\ status[t] = "stopping" /\ t \notin agents /\ t \notin compilers
    /\ journal # {t} /\ status' = [status EXCEPT ![t] = "resolved"]
    /\ UNCHANGED <<agents, compilers, expired, fresh, journal, files, accepted, phaseAdvanced, crashed>>

Advance ==
    /\ ~crashed /\ ~phaseAdvanced /\ Occupied = {} /\ journal = {}
    /\ phaseAdvanced' = TRUE
    /\ UNCHANGED <<status, agents, compilers, expired, fresh, journal, files, accepted, crashed>>

CrashOrRecover ==
    /\ crashed' = ~crashed
    /\ UNCHANGED <<status, agents, compilers, expired, fresh, journal, files, accepted, phaseAdvanced>>

Next == Advance \/ CrashOrRecover \/ \E t \in Tasks :
    Reserve(t) \/ Launch(t) \/ Check(t) \/ ReapCompiler(t) \/ Finish(t) \/ Expire(t)
    \/ Stale(t) \/ Reject(t) \/ Prepare(t) \/ PublishFiles(t) \/ PublishState(t)
    \/ Receipt(t) \/ ReapAgent(t) \/ Resolve(t)
Spec == Init /\ [][Next]_vars

Capacity == Cardinality(Occupied) <= WorkerSlots /\ Cardinality(compilers) <= CompilerSlots
Reservations == \A t, other \in Occupied : <<t, other>> \notin Conflicts
ReapedBeforeRelease == agents \cup compilers \subseteq Occupied
AcceptanceFresh == accepted \subseteq fresh \ expired
RecoverablePublication == accepted \subseteq files /\ files \ accepted \subseteq journal
PhaseSafety == phaseAdvanced => Occupied = {} /\ compilers = {} /\ journal = {}
=============================================================================
