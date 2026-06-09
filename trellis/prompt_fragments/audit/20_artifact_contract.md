## Artifact contract

Emit a single JSON object matching the schema below. The kernel validates each field; malformed responses get one retry per burst, then force `audit_done`.

```json
{
  "new_tasks": [
    {
      "target_node": "SomeNodeId",
      "rationale": "free-form audit reasoning, ~1-3 sentences",
      "confidence": "high",
      "kind": {
        "kind": "substitution",
        "replacement": {
          "kind": "mathlib",
          "citation": "Nat.add_comm"
        }
      }
    },
    {
      "target_node": "OtherNodeId",
      "rationale": "wrapper of ExistingTabletNode; argument lists match",
      "confidence": "medium",
      "kind": {
        "kind": "substitution",
        "replacement": {
          "kind": "tablet_wrapper",
          "node": "ExistingTabletNode"
        }
      }
    },
    {
      "target_node": "ThirdNodeId",
      "rationale": "unused variable warning on line 14",
      "confidence": "high",
      "kind": {
        "kind": "lint_fix",
        "warning_text": "unused variable 'x' [linter.unusedVariables]"
      }
    }
  ],
  "task_modifications": [
    {"task_index": 7, "reason": "second-look: not a wrapper after all, the lemma takes different arguments"}
  ],
  "scratchpad_replace": "burst 2 notes:\n- Searched mathlib for `Foo.bar`; doesn't exist\n- Considered substituting QuuxLemma; the conclusion differs in subtle ways, leaving it",
  "outcome": "need_to_continue"
}
```

### Field semantics

- **`new_tasks`** — append-only. The kernel adds each entry to `cleanup_audit_tasks` with `status: Pending` and `audit_origin_round` set to the current round. Empty array is legal (a burst that revises prior tasks but proposes no new ones).
- **`task_modifications`** — revisions to Pending tasks (your current-round proposals, or leftover Pending tasks from a prior round). Each entry transitions Pending → Dismissed with the provided reason. Empty array is legal.
- **`scratchpad_replace`** — replaces the entire scratchpad (not appended). Empty string clears the scratchpad.
- **`outcome`** — `"audit_done"` or `"need_to_continue"`. The kernel forces `audit_done` if `burst_count == max_bursts_per_round`, regardless of what you set here.

### What the kernel rejects

- A `new_tasks` entry whose `target_node` is not in `current_present_nodes` or is in `protected_statement_node_set`.
- A `Substitution.replacement.tablet_wrapper.node` not in `current_present_nodes`.
- A `Substitution.replacement.mathlib.citation` that is empty/whitespace.
- A `LintFix.warning_text` that is empty/whitespace.
- A `(target_node, kind)` pair duplicating an existing task.
- A `task_modifications` entry whose `task_index` is out of range, or refers to a non-Pending task.

A rejected burst gets one retry: the kernel re-issues the audit request with `latest_audit_rejection_reason` populated. On second consecutive rejection (or a second consecutive Malformed response), the kernel forces `audit_done` and transitions to the reviewer with whatever tasks have been validly accumulated in prior bursts.

### Empty responses

It is fully legitimate to return:
```json
{
  "new_tasks": [],
  "task_modifications": [],
  "scratchpad_replace": "",
  "outcome": "audit_done"
}
```
if there is genuinely nothing to clean up. The cleanup phase will then exit immediately into `Phase::Complete` (after a trivial reviewer cycle).
