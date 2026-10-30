--------------------------- MODULE PvRevision ---------------------------
EXTENDS Naturals, Sequences
\* Bounded abstraction of the immutable kernel handoff and external managed
\* revision lifecycle. A successor has a new generation and fresh Modeling.
VARIABLES enabled, isPv, disprove, sourceWitness, generation, phase,
          source, goals, frozen, candidate, approved, scoped, task,
          modeled, cleanupEmitted
vars == <<enabled, isPv, disprove, sourceWitness, generation, phase,
          source, goals, frozen, candidate, approved, scoped, task,
          modeled, cleanupEmitted>>
NoBinding == <<2, 2>>
RevisionPhases == {"revision", "human", "successor"}
Init == /\ enabled \in BOOLEAN /\ isPv \in BOOLEAN
        /\ disprove \in BOOLEAN /\ sourceWitness \in BOOLEAN
        /\ generation = 0 /\ phase = "proof"
        /\ source = 0 /\ goals = 0 /\ frozen = <<>>
        /\ candidate = <<0, 0>> /\ approved = NoBinding /\ scoped = NoBinding
        /\ task = "pending" /\ modeled = TRUE /\ cleanupEmitted = FALSE
\* Explicit stopped-boundary opt-in leaves the existing generation unchanged.
ActivateRevision ==
    /\ phase = "proof" /\ isPv /\ ~enabled /\ enabled' = TRUE
    /\ UNCHANGED <<isPv, disprove, sourceWitness, generation, phase,
                   source, goals, frozen, candidate, approved, scoped, task, modeled, cleanupEmitted>>
\* All worker/verifier/reviewer/StartCycle auto-advance routes use this branch.
FormalizationComplete ==
    /\ phase = "proof"
    /\ IF enabled /\ isPv /\ disprove
       THEN /\ phase' = "revision"
            /\ frozen' = Append(frozen, <<source, goals>>)
            /\ candidate' = <<source, goals>>
       ELSE /\ phase' = "cleanup" /\ UNCHANGED <<frozen, candidate>>
    /\ UNCHANGED <<enabled, isPv, disprove, sourceWitness, generation,
                   source, goals, approved, scoped, task, modeled, cleanupEmitted>>
EditCandidate ==
    /\ phase \in {"revision", "human"}
    /\ candidate' \in {<<s, g>> : s \in 0..1, g \in 0..1}
    /\ approved' = NoBinding /\ scoped' = NoBinding /\ task' = "pending"
    /\ phase' = "revision"
    /\ UNCHANGED <<enabled, isPv, disprove, sourceWitness, generation,
                   source, goals, frozen, modeled, cleanupEmitted>>
Scope ==
    /\ phase = "revision" /\ task' \in {"passed", "failed"}
    /\ IF task' = "passed" THEN scoped' = candidate ELSE scoped' = NoBinding
    /\ phase' = IF task' = "passed" THEN "human" ELSE "revision"
    /\ UNCHANGED <<enabled, isPv, disprove, sourceWitness, generation,
                   source, goals, frozen, candidate, approved, modeled, cleanupEmitted>>
ApproveExactSourceAndRequirements ==
    /\ phase = "human" /\ task = "passed" /\ scoped = candidate
    /\ approved' = candidate /\ phase' = "successor"
    /\ UNCHANGED <<enabled, isPv, disprove, sourceWitness, generation,
                   source, goals, frozen, candidate, scoped, task, modeled, cleanupEmitted>>
PrepareSuccessor ==
    /\ phase = "successor" /\ generation < 2
    /\ task = "passed" /\ approved = candidate /\ scoped = candidate
    /\ source' = candidate[1] /\ goals' = candidate[2]
    /\ generation' = generation + 1 /\ phase' = "modeling" /\ modeled' = FALSE
    /\ disprove' \in BOOLEAN
    /\ UNCHANGED <<enabled, isPv, sourceWitness, frozen, candidate,
                   approved, scoped, task, cleanupEmitted>>
ApproveModeling ==
    /\ phase = "modeling" /\ phase' = "argument" /\ modeled' = TRUE
    /\ UNCHANGED <<enabled, isPv, disprove, sourceWitness, generation,
                   source, goals, frozen, candidate, approved, scoped, task, cleanupEmitted>>
ArgumentAndProof ==
    /\ phase = "argument" /\ modeled /\ phase' = "proof"
    /\ approved' = NoBinding /\ scoped' = NoBinding /\ task' = "pending"
    /\ UNCHANGED <<enabled, isPv, disprove, sourceWitness, generation,
                   source, goals, frozen, candidate, modeled, cleanupEmitted>>
Cleanup ==
    /\ phase = "cleanup" /\ phase' = "complete" /\ cleanupEmitted' = TRUE
    /\ UNCHANGED <<enabled, isPv, disprove, sourceWitness, generation,
                   source, goals, frozen, candidate, approved, scoped, task, modeled>>
Next == ActivateRevision \/ FormalizationComplete \/ EditCandidate \/ Scope \/ ApproveExactSourceAndRequirements
        \/ PrepareSuccessor \/ ApproveModeling \/ ArgumentAndProof \/ Cleanup
Spec == Init /\ [][Next]_vars
NoRefutedCompletion == enabled /\ isPv /\ disprove => phase # "complete"
NoRevisionCleanup == phase \in RevisionPhases => ~cleanupEmitted
FrozenGeneration == phase \in RevisionPhases => frozen[Len(frozen)] = <<source, goals>>
ExactApproval == phase = "successor" => approved = candidate /\ scoped = candidate /\ task = "passed"
FailedTaskCannotAdvance == task = "failed" => phase = "revision"
FreshModelingGate == phase = "modeling" => ~modeled
ModelingRequired == phase \in {"argument", "proof", "cleanup", "complete"} => modeled
=============================================================================
