## Artifact contract

Raw artifact: one JSON object matching this schema.

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
    },
    {
      "target_node": "FourthNodeId",
      "rationale": "long proof with several context facts that appear unused",
      "confidence": "medium",
      "kind": {
        "kind": "dead_code_elim",
        "hint": "the preliminary haux/hcase block after the second branch"
      }
    },
    {
      "target_node": "FifthNodeId",
      "rationale": "1800 lines; the third case split stands alone as a lemma",
      "confidence": "high",
      "kind": {
        "kind": "extract_helper",
        "ordinal": 1,
        "hint": "the reasoning under the third case split"
      }
    },
    {
      "target_node": "CanonicalLeastParent",
      "rationale": "the same proof region occurs in three nodes",
      "confidence": "medium",
      "kind": {
        "kind": "extract_shared",
        "co_parents": ["OtherParent", "ThirdParent"],
        "ordinal": 1,
        "hint": "region id and the intermediate facts the helper may need to return"
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
- A `DeadCodeElim` whose `target_node` carries no proof.
- An `ExtractHelper` whose `ordinal` is below 1, or whose `target_node` carries no proof.
- An `ExtractShared` with fewer than two declared parents, an ordinal below 1, a non-proof/protected/missing parent, or a non-canonical target. The kernel canonicalizes the full parent set so its lexicographically least member is `target_node`; the rest are `co_parents`. There is no parent cap or minimum detected block size.
- A `(target_node, kind)` pair duplicating an existing task — `ExtractHelper` is keyed `(target_node, ordinal)`, and `ExtractShared` by its full parent set plus ordinal.
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
