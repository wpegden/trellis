## Kernel-authored Modeling substantiveness rubric

{{rubric_json}}

- `Pass`: the statement satisfies the Modeling rubric. A supplied GOAL can pass
  even when there is evidence it is false: faithful statement precedes proof or
  disproof. Assess supporting content for usefulness and justified hypotheses.
- `FalseAsStated`: a substantive, faithful statement has a concrete refutation;
  record that evidence in the comment. For a supplied GOAL not used as model
  support, this does not block Modeling or authorize changing its meaning. For
  preservation or proof support, it remains a blocker requiring repair.
- `Fail`: the statement is unsuitable, for example through vacuity, duplication,
  wrong domain, missing hypotheses, or unjustified weakening, strengthening, or
  reversal of a supplied obligation. Give a repair preserving the GOAL's meaning.
- `NotDoneYet`: careful evaluation remains pending.

Set `decision: PASS` when every verdict is `Pass`, `FalseAsStated`, or
`NotDoneYet`. Set it to `FAIL` when any verdict is `Fail`. This response-level
decision does not waive the kernel's per-node verification gates.
