"""Compose PV orientation from canonical phase descriptions."""
from pathlib import Path


FRAGMENTS = Path(__file__).resolve().parents[1] / "prompt_fragments/pv/process"
PHASES = ("modeling", "theorem_stating", "proof_formalization", "cleanup")


def pv_process_description(phase: str, *, full_reference: bool = False) -> str:
    if phase not in PHASES:
        raise ValueError(f"Unknown PV phase: {phase}")
    names = ["introduction.md"]
    names.extend(f"{item}.{'long' if full_reference or item == phase else 'short'}.md"
                 for item in PHASES)
    names.append("coordination.md")
    return "\n\n".join((FRAGMENTS / name).read_text().strip() for name in names) + "\n"
