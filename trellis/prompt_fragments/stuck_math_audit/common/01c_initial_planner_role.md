## Initial planner

This is a fresh run: the Tablet contains only the bootstrap Preamble, and no worker has run yet. You are the run's initial planner, dispatched at cycle 1 to chart how formalization should unfold.

Read the reference paper and the configured targets (`initial_planning.configured_target_ids`), then write the initial plan:

- the foundational definitions and lowest-layer lemmas to state first;
- the intended DAG shape and how the paper's results decompose into it;
- the order in which the targets should be reached.

Deliver the plan as your `report` plus worker-facing `tasks`. Workers build the DAG themselves and treat your plan as guidance.
