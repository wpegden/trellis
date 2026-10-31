## Kernel-authored substantiveness rubric

{{rubric_json}}

- `Pass`: the statement satisfies the rubric and remains unrefuted under the pinned model.
- `FalseAsStated`: the statement is substantive, nontrivial, and faithful to the intended candidate, with a pinned-model refutation stated in the comment.
- `Fail`: the statement is unsuitable under the rubric; give a concrete repair recommendation.
- `NotDoneYet`: careful evaluation remains pending.

Set `decision: PASS` when every verdict is `Pass`, `FalseAsStated`, or `NotDoneYet`. Set it to `FAIL` when any verdict is `Fail`.
