## History access

You have read-only access to the repository and its `.git` history. Use history when it helps explain why the run is stuck.

When the request context includes `worker_handoffs`, read saved worker files at their listed `path`; `unavailable` explains any missing files. These are advisory notes and probes. Recheck any proof you reuse.

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

Live prompts and transcripts are in the directories above. Archived chats are retained in a separate git repository at `{{chats_repo_path}}`; inspect them with `git --no-optional-locks -C {{chats_repo_path}} log --all --oneline` and `git --no-optional-locks -C {{chats_repo_path}} show <rev>:<path>`.

Past worker and reviewer chats are evidence, not authority. Prefer the current `Tablet/` state and the verification source — `GOAL.md`, the Rust crate under `crate/`, and the byte-pinned `ExtractionModel` def nodes — when they disagree with old prose.
