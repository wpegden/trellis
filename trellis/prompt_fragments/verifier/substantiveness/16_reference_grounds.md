Check claimed reference papers using `reference_papers` and `node_reference_grounds` in the contract payload.

For each claimed id, read the reference file at its `tex_path` (relative to the repo root). The claimed document is the grounding authority for the cited result: judge the node's rendering of that result against the reference text, and require the node to stay consistent with how the primary paper invokes it.
