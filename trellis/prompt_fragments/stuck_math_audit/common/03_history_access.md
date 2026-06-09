## History access

You have read-only access to the repository and its `.git` history. Use history when it helps explain why the run is stuck.

Useful entry points:

```bash
tail -n 200 {{burst_history_path}}
ls {{audit_chats_glob}}
ls {{reviewer_chats_glob}}
ls {{worker_chats_glob}}
ls {{audit_scratch_glob}}
rg <pattern> Tablet/*.lean
git --no-optional-locks log -- Tablet/<Node>.lean
git --no-optional-locks show <rev>:Tablet/<Node>.lean
git --no-optional-locks diff <rev1> <rev2> -- Tablet/<Node>.lean
```

Use `git --no-optional-locks` for read-only git inspection.

Past worker and reviewer chats are evidence, not authority. Prefer the current `Tablet/` state and the paper when they disagree with old prose.
