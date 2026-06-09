This task comes after unresolved per-node substantiveness. See `SUBSTANTIVENESS.md` (inlined elsewhere in this prompt) for the canonical rubric.

Your job is to repair the failing node(s) by rewriting the `.tex` so the rubric passes. Do not patch surface symptoms (renaming, light rewording). The fix usually requires:

- **For Clause 1 failures:** strengthen the statement to match the full statement actually used by the paper's proof/approach.
- **For Clause 2 failures:** merge the redundant content into the node it duplicates, or retarget consumers so they no longer use the redundant node. Only delete an existing node when the kernel-authored scope/checker allows deletion; otherwise remove or replace the dependency and let automatic orphan cleanup remove the node later if it becomes unsupported.
