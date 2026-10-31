# Authoring a PV seed

A program-verification seed contains the goals and source from which Trellis
starts a campaign. This guide explains what to supply and which work Trellis
handles after launch.

Goal text is fixed for a campaign. Trellis can revise its formalization, but
cannot silently replace the intended requirements. Review the goals before
launch and record later corrections as an explicit revision.

See [Writing a PV GOAL document](PV_GOAL_AUTHORING.md) for the supported goal
structures and statement contents.

## Choosing the goal input

**Prose mode** accepts natural-language properties. Native modeling accepts a
JSON collection with a separate ID for each property and generates `GOAL.md`.
The callable-contract launcher accepts Markdown with one function contract per
heading. Choose the format supported by your launch route.

Trellis writes the formal statements, mathematical definitions, and any
semantics needed for operations left opaque by extraction. It reviews whether
these artifacts faithfully express the supplied goals and source. You do not
need to author Lean for a prose seed.

**Spec mode** accepts hand-authored Lean theorem statements. These statements
receive correspondence and substantiveness waivers: their agreement with your
intent is your responsibility. A machine-checked proof establishes the formal
statement supplied, so a mistaken statement can still have a valid proof.

In either mode, the extraction model is generated from the source by Charon
and Aeneas. It is distinct from the authored specification.

## Uploading upstream source

Supply the upstream module or crate you want verified, together with any
sibling files its imports need. Upload the source unmodified; do not add a
manifest or adapt the program for verification. Trellis handles that setup.

The callable-contract launcher provides two source slots:

- **Target source** contains the module directory or crate being verified.
- **Supporting upstream files** optionally contains unmodified sibling files
  from the same upstream crate, as one `.tar.gz` or several loose files.

Agent choices are launch settings, not seed content. The PV launcher obtains
their defaults from the explicit PV config template, leads with a launch preset
(astra-guided sol, all sol, sol-guided luna, all luna), and exposes each
Phase-0 and in-run model/effort binding behind "show per-role settings". A
guided preset puts the guide model on the stuck-math audit and the relevant
preparation or Phase-0 audit lanes; an "all X" preset puts
X on every lane. Lanes may share the adapting worker's model:
`phase0.allow_same_model_lanes` is on in the template and the launcher, and
genesis records it.

Keep the source's relative paths and avoid supplying the same path in both
slots. The launcher displays potentially missing imports to help you complete
the upload. Trellis cannot fetch missing files from the network. If adaptation
requires a replacement for an unavailable dependency, Trellis records that
replacement and presents it for review.

For the callable-contract launcher, every function heading must name a
function in the target source. Functions supplied only as supporting files
are not selectable targets through that format.

Agent models and effort levels are launch settings. The launcher supplies
defaults and exposes them for selection. The independent-goals launcher defaults
preparation and main runtime roles to Sol at xhigh, and grunts to Luna at high;
roles may share a model. The callable-contract launcher also permits roles to share a model.

## Writing the goals

Describe the program's intended behavior in ordinary language. State any
application-specific assumptions or meanings needed to understand a claim;
you do not need to duplicate source signatures or setup records.

A documented guarantee, an explicit owner requirement, or a justified helper
contract can establish the purpose of a behavioral goal. Identify diagnostic
implementation properties and supporting mathematical properties according
to their roles. Observing what the code currently does does not by itself
establish what it ought to do.

An intended guarantee may turn out to be false. That is a useful verification
outcome. Do not weaken a requirement simply because proving it is difficult
or because a counterexample has appeared.

Give each property that needs a separate result its own entry where the
chosen format supports this. In the callable-contract format, clauses under
one heading form one contract; a refutation of that contract does not settle
each clause independently.

## What Trellis handles

Trellis resolves functions from the source, records the source and execution
configuration, develops formal definitions, and reviews the resulting
specification. Consequential ambiguities and assumptions are surfaced for
review rather than silently changing the intended claim.

The callable-contract launcher also selects its verification method from the
function's supported types and effects. Some functions support execution-based
counterexample evidence; others use the proof route. Selecting the proof route
does not mean the property has already been established.

At the human approval gate, Trellis presents the formal statements and their
relevant definitions, assumptions, and source correspondence. Approval fixes
their meaning for subsequent proof work. A later refutation concerns that same
approved meaning.

[PV_GOAL_REVIEW.md](PV_GOAL_REVIEW.md) describes the detailed responsibilities
of the process and its reviewers.

## Author's checklist

- Supply unmodified target source and the upstream files it needs.
- Use the documented goal structure for your launch route.
- State the intended properties and any necessary application assumptions.
- Include referenced contract text needed to interpret the goals.
- For prose mode, leave the formal definitions and proofs to Trellis.
