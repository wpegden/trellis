------------------------- MODULE ModelingApproval -------------------------
EXTENDS Naturals
\* Focused abstraction: the large supervisor model begins after Modeling.
\* Meaning abstracts the existing native Lean + NL correspondence fingerprint;
\* argument abstracts proof sketches and bodies excluded from that fingerprint.
VARIABLES phase, meaning, approved, argument, ratifications, lastRatified
vars == <<phase, meaning, approved, argument, ratifications, lastRatified>>
Init == /\ phase = "modeling" /\ meaning = 0 /\ approved = 0
        /\ argument = 0 /\ ratifications = 0 /\ lastRatified = 0
Model == /\ phase = "modeling" /\ meaning' \in 0..2
         /\ UNCHANGED <<phase, approved, argument, ratifications, lastRatified>>
ApproveModel == /\ phase = "modeling" /\ phase' = "argument"
                /\ approved' = meaning /\ lastRatified' = meaning
                /\ ratifications' = ratifications + 1 /\ UNCHANGED <<meaning, argument>>
Work == /\ phase \in {"argument", "proof"} /\ argument' \in 0..2
        /\ meaning' = meaning
        /\ UNCHANGED <<phase, approved, ratifications, lastRatified>>
Advance == /\ phase = "argument" /\ phase' = "proof"
           /\ UNCHANGED <<meaning, approved, argument, ratifications, lastRatified>>
RequestRevision == /\ phase \in {"argument", "proof"} /\ phase' = "revision"
                   /\ UNCHANGED <<meaning, approved, argument, ratifications, lastRatified>>
Revise == /\ phase = "revision" /\ meaning' \in 0..2
          /\ UNCHANGED <<phase, approved, argument, ratifications, lastRatified>>
ApproveRevision == /\ phase = "revision" /\ phase' = "proof"
                   /\ approved' = meaning /\ lastRatified' = meaning
                   /\ ratifications' = ratifications + 1 /\ UNCHANGED <<meaning, argument>>
\* A later explicit review may add conditional scope; the original meaning
\* remains protected. New companion meanings are outside this abstraction.
ReviewAdditional == /\ phase = "argument" /\ phase' = "proof"
                    /\ ratifications' = ratifications + 1
                    /\ UNCHANGED <<meaning, approved, argument, lastRatified>>
Next == Model \/ ApproveModel \/ Work \/ Advance \/ RequestRevision \/ Revise \/ ApproveRevision \/ ReviewAdditional
Spec == Init /\ [][Next]_vars
ApprovedMeaningPreserved == phase \in {"argument", "proof"} => meaning = approved
NoRetroactiveRatification == approved = lastRatified
=============================================================================
