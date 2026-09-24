## Kernel-authored soundness contract

{{contract_json}}

The `summary`, `comments`, and `soundness.explanation` fields in your artifact are reviewer-facing. Use them to explain your lane's reasoning briefly and concretely, especially for `UNSOUND` or `STRUCTURAL` judgments.

For an `UNSOUND` or `STRUCTURAL` judgment, after identifying the decisive gap, continue checking the rest of the proof and enumerate in `comments` every independent blocking gap you found, numbered, so a single repair burst can address all of them.
