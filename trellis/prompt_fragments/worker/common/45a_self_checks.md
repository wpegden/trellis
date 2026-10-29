## Self-Checks Before `valid`

Every live node has target support or imports from target support: a node your edits leave without it must regain support or go in `deleted_nodes` (waived while any configured paper target lacks a covering node), and only unsupported nodes may be deleted.

If you edit or add a proof-bearing `.tex` node that is not `.lean`-closed and doesn't have a `SKETCH:` marker and return `valid`, you are claiming the relevant NL proof is expected to pass the soundness verifier.

If you add a node or edit the `.tex` or `.lean` statements of a node, you are claiming the node is expected to pass the correspondence verifier.

If you add a node or edit the `.tex` statement of a node, you are claiming the node is expected to pass the substantiveness verifier.

In particular, before returning `valid`, adversarially audit nodes you've added or edited (but not other nodes) against `SUBSTANTIVENESS.md`, `CORRESPONDENCE.md`, and (if the node is not `.lean`-closed and doesn't have a `SKETCH:` marker) `SOUNDNESS.md`. Handle any concerns surfaced by this audit before returning valid (consider dispatching the audit in the background while you wait, e.g., on the deterministic checker).

For the soundness audit, tabulate each outside fact the NL proof uses: the `\noderef`-cited node whose *statement* states it, with the clause quoted after re-reading that `.tex` statement. Record the table (or the gaps it exposed) in scratch or `comments`. An existence/`Nonempty` citation supplies existence only, and a fact proved inside another node's proof becomes citable once it has a statement-level home.
