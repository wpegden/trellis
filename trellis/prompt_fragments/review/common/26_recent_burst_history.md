## Recent burst history

Append-only JSONL of every worker/reviewer/verifier response across the run is available here:

```
.trellis/logs/burst-history.jsonl
```

One row per response, with `cycle`, `kind`, `active_node`, `mode`, and response-kind fields (`outcome` for workers; `decision` / `next_mode` / `must_close_active` / `allow_new_obligations` / `reset` / `comments` for reviewers).

Useful when deciding whether a recurring decision on the current active node is actually working — `tail` or `grep '"active_node":"<X>"' .trellis/logs/burst-history.jsonl` to see prior attempts. Not authoritative — the contract above is. Absent file = no history yet; proceed from the contract.

Always check recent history before deciding next steps. If work has been stuck/frustrated across many cycles, be willing to authorize a broader restructure if appropriate, even if such a move appears destructive in terms of raw closure counts. Keep in mind that it may well be that formalization is otherwise blocked and can cycle indefinitely.

## Tablet git history

The live `Tablet/` is a git worktree. Its history is preserved across `last_clean` rewinds (rewinds restore the worktree from a tagged commit; they don't rewrite history). Each cycle leaves a checkpoint commit with subject `supervisor2 checkpoint NNNNNN | cycle N | <phase> | <stage> | <active_node>`. The repo is mounted read-only into your sandbox, so any `git` subcommand that doesn't mutate state will work.

Useful read-only operations (run from the repo root, which is your working directory):

```
git log --oneline -- Tablet/<NodeName>.lean        # which cycles touched this node
git show <commit>:Tablet/<NodeName>.lean           # contents at a prior commit
git diff <commit1>..<commit2> -- Tablet/           # what changed between two cycles
git log --grep='cycle <N>' --oneline               # find the commit for a specific cycle
```

Especially useful after a `last_clean` rewind: the rewound-away path is still inspectable in git, so you can see what the worker tried last time and what blockers it hit before deciding what to try differently from the clean state.
