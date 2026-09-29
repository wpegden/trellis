## Cone clean

When the current run is blocked by a problematic decomposition across several
files, the best course of action is often to restore this section of the DAG to
the state that came out of the theorem-stating phase.

You can call for this by setting `cone_clean_node` to one allowed coarse node. 
The runtime will restore that node to its theorem-stating files and prune helper
nodes that become orphaned. Write the report and tasks for rebuilding from that
restored skeleton.

To estimate impact, locate the `cone_clean_impact.py` helper under the
trellis source tree's `scripts/` directory (bind-mounted read-only into
this sandbox; the exact host path varies by deployment) and run it with
`--context-json {{context_json_path}} --node N`. If you cannot locate the
script, derive the impact directly from the current decomposition: restore
`N`'s theorem-stating imports and target claims, compute the dependency
closure from target roots, and enumerate the nodes that would be pruned
(those orphaned outside the closure).

Note that a cone clean may well be the best course of action even in cases where many
good nodes would be discarded by the operation. The autoformalization process
is highly local and is thus much better and faster at building a good decomposition
than correcting a bad decomposition. To a rough approximation, if you expect
total LOC in bad nodes that would be discarded is close to or exceeds total LOC 
in good nodes that would be discarded, favor cone clean. Favor it even more if
the process has failed to self-correct despite previous audits.
