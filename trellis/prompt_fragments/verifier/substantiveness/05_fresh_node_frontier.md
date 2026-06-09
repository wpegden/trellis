This lane is reviewing substantiveness on a fresh frontier of nodes. The canonical rubric is in `SUBSTANTIVENESS.md` (inlined elsewhere in this prompt).

For each node listed in `request_summary.nodes`, render `Pass` / `Fail` / `NotDoneYet` against the rubric. Making this judgment naturally involves referencing the node itself, other nodes in the tablet (especially those it imports or vice versa), and the tex paper being formalized.

Triage with explicit verdicts: emit a `Pass` / `Fail` / `NotDoneYet` verdict for every node in `request_summary.nodes`. Silence is treated as `NotDoneYet`, so be explicit about Pass nodes — don't omit them. Mark a node `NotDoneYet` if you did not have time to read it carefully; the kernel will re-issue another Paper request covering exactly the NotDoneYet residual.

When you Fail a node, give a concrete next-step recommendation in `verdicts[].comment` (required). Comments on Pass and NotDoneYet are optional.
