-- [TABLET NODE: AxiomForge]
import Tablet.Preamble

/-- FIX 1 regression: a node authoring a reserved-shaped AXIOM. Before the
    fix, the transparent-walk branch's `.axiomInfo` arm did `pure ()`, so
    `AxiomForge.eq_1` was dropped from `kernel_axioms` entirely — a
    consumer could derive it with no axiom surfaced (the provable-`False`
    path when the axiom is `False`). We use a true Prop so the FILE builds;
    the point is that the axiom must SURFACE in `kernel_axioms`. The
    principal `AxiomForge` references the axiom so the consumer's closure
    reaches it. (FIX 2 separately rejects this node at its OWN acceptance,
    since `eq_1` is a reserved-shaped authored name.) -/
axiom AxiomForgeEq1 : 2 + 2 = 4

theorem AxiomForge : 2 + 2 = 4 := AxiomForgeEq1

axiom AxiomForge.eq_1 : 2 + 2 = 4
