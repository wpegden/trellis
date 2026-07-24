<!-- PROPOSED 2026-06-14: pending owner review -->
# Restructure scope

This is a **restructure** dispatch: the reviewer authorized a coordinated restatement that reaches across the usual focus cone. The nodes named in `scope_contract.authorized_existing_nodes` are yours to restate — edit each one's `.tex` statement, its pre-`-- BODY` Lean signature (hypotheses or return type), and its proof-so-far, and rewire dependencies among them as the restatement requires. You may also add new helper nodes that the restatement needs.

Every statement you change re-enters correspondence and substantiveness verification, so make each restated statement faithful to the source and substantive on its own terms; the verifiers judge the result.

Statements outside `authorized_existing_nodes` stay as they are — approved-target, challenge byte-pinned, and `Preamble`/`Axioms` statements are frozen and absent from your authorized set. Keep edits within the authorized set; when an honest restatement needs a node the reviewer did not authorize, return `needs_restructure` and name that node so the reviewer can re-issue with a wider `authorized_node_ids`.
