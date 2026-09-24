This review is the first burst of a new phase. The previous reviewer's `AdvancePhase` decision has been ratified by the human gate — there is no prior worker burst in this phase to evaluate.

Your job is to route the first worker burst: pick `next_active` from the legal candidate set, set `next_mode`, and choose `must_close_active` and `allow_new_obligations` deliberately. The kernel does not pick defaults here; whatever you set seeds the first burst.
