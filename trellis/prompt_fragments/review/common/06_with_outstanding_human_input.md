Human or expert input is currently outstanding in this review state.

The free-text guidance the human/expert wrote is NOT carried in the structured request — it lives in the file `HUMAN_INPUT.md` at the repository root (read-only, inside your workspace). Read it now (e.g. `cat HUMAN_INPUT.md`) to learn the actual concern before you decide. `HUMAN_INPUT.md` is the single source of truth for what the human wrote; do not conclude "no payload exists" from the structured request alone.

Account for that input explicitly. If the concern has been addressed, use the contract's clearing mechanism. If it has not, do not silently route around it; make the next task or escalation reflect that open external guidance.

Only set `clear_human_input=true` when the outstanding concern is already addressed and no repair worker is needed for that concern. If you are assigning worker work to address the concern, leave `clear_human_input` false so the kernel keeps the human/expert guidance outstanding until the repair has run.
