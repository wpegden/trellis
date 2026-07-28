## Additional reference papers

The operator has registered additional reference papers as grounding authority for results the primary paper cites. The registry is in `request_summary.reference_papers` (id -> `{tex_path, source_id}`); each `tex_path` is relative to the repo root and directly readable. The primary paper remains the sole authority for targets and statements.

When a node's statement leans on a cited result from one of these documents, declare it in your result JSON: `node_reference_grounds` maps the node to its FULL set of reference-paper ids (full replacement per node; the kernel rejects ids outside the registry). Current claims are in `request_summary.node_reference_grounds`.
