## Source recourse

The Trellis source snapshot is mounted read-only at `{{reviewer_source_snapshot_path}}` at git SHA `{{reviewer_source_sha}}`.

When preparation instructions or tooling leave no legal way to proceed, consult `trellis/modeling/driver.py`, `trellis/modeling/native.py`, and `scripts/pv_modeling.py` to understand the preparation checks and transitions. This is source context for preparation; no Modeling worker acceptance decision has occurred yet. Use the current task, recorded observations, and audit requirements as the decision contract, and report implementation defects in `system_feedback`.
