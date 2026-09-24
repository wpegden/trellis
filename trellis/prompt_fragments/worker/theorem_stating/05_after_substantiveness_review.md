This task comes after unresolved per-node substantiveness. See `SUBSTANTIVENESS.md` (inlined elsewhere in this prompt) for the canonical rubric.

Your job is to repair the failing node(s) by rewriting the `.tex` so the rubric passes. Do not patch surface symptoms (renaming, light rewording). The fix usually requires:

- **For Clause 1 failures:** strengthen the statement to match the full statement actually used by the paper's proof/approach.
- **For Clause 2 failures:** merge duplicate content or retarget consumers. Delete unsupported nodes only when scope/checker allows it, and list them in `deleted_nodes`.
- **If your node claims a deviation in `rejected_deviations`:** the Deviation lane rejected it — drop the claim or revise its `.tex` to re-verify.
