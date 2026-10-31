# Developing and reviewing a PV specification

This guide is for the process agents and reviewers developing a PV
specification. The verification process develops explicit requirements from
the GOAL and available contract evidence. Agents and reviewers record the
details needed to connect the resulting claims to the program and assess
their coverage.
[PV_GOAL_AUTHORING.md](PV_GOAL_AUTHORING.md) describes the initial author input.

## Subject and evidence

Record the uploaded source identity and available provenance, resolved
callables, generic specializations, toolchain, build settings, and execution
environment. Distinguish mechanically recorded facts from interpretations
that require review. Identify the exact documentation or requirements used
to elaborate the GOAL.

Documentation, design comments, and explicit owner requirements can establish
intended behavior. Merely observing how the implementation behaves does not
establish a correctness criterion. Classify implementation-specific claims
as diagnostic or auxiliary where appropriate.

Resolve routine details from the supplied material. Surface consequential
ambiguities that remain, and record their resolution. Preserve the intended
scope when selecting callable bindings or adapting the program.

## Explicit requirements

The reviewed specification should identify, where relevant:

- Input and state domains.
- Caller and environment preconditions, with their justification.
- Observable results, including errors and permitted state changes.
- Termination and exceptional-behavior requirements.
- The relationship between public behavior and supporting helper contracts.

Validate the structured inventory before formalization: each independently
commissioned property must have its own registered target ID, and the issued
inventory must match the supplied goal collection. Record explicit exclusions
or declines. A prose instruction to track clauses cannot create target IDs.
For the callable-contract launcher, each function heading is one target;
multiple clauses remain a single contract.

Keep partial correctness distinct from progress: a claim about normal returns
does not establish termination or absence of panics. Likewise, a claim about
successful calls does not ensure that all valid inputs succeed.

Derive internal preconditions from established contract boundaries. Verify
that callers establish them. A helper's restricted domain need not restrict
the public entry's domain. Do not introduce bounds or exclude cases solely
because a proof is difficult or an intended claim has a counterexample.

## Definitions and correspondence

Develop mathematical meanings independently of the behavior being tested.
An implementation's approximation can support a diagnostic specification,
but the public correctness statement must retain the intended meaning.

Identify the observations used by each claim: values, bit patterns, errors,
state changes, traces, or internal representations. For adapted interfaces,
establish the exact relationship to the source observations. A projection
must not hide distinctions that an obligation investigates.

Check the well-posedness of authored vocabulary, including existence and
uniqueness where needed. Such mathematical properties support the program
specification; source agreement with that specification is a separate
obligation. Mathematical domains may be broader than executable input domains
when their roles are explicit.

Keep extraction choices, boundary semantics, and preservation evidence in
the setup and correspondence records. Their justification must be independent
of the correctness claims the campaign is investigating.

## Coverage and composition

Give independently valuable questions separate identities and report their
status separately. A conjunction may summarize them, but one counterexample
refutes the conjunction without settling its other components. Review the
generated inventory against every supplied requirement.

Check composition arguments for meaningful use of their premises. A false
contract for a concrete component can make a conditional correctness theorem
provable by contradiction while establishing nothing about the composition.

Review the content of the formal statements themselves. An explanation in a
proof narrative cannot supply a requirement missing from the statements or
repair disagreement between the natural-language and Lean statements. A
closed Lean proof establishes its proposition; correspondence and
substantiveness review must still judge whether that proposition expresses
the supplied goal. The ordinary waiver of natural-language proof review for
a closed Lean proof does not waive either responsibility.

A modular approach is to establish composition for abstract components
satisfying coherent contracts, then separately verify the actual components
against those contracts. Include every relevant execution path and the
mathematical or representation arguments that connect them. A failed concrete
instantiation must leave the corresponding public guarantee unresolved even
when the abstract composition theorem is proved.

Supporting obligations should serve the supplied intent. New public
requirements need a basis in that intent rather than merely being interesting
properties to investigate. Diagnostic claims that characterize an internal
approximation need their own interpretation of success and refutation.

## Review checklist

- Does each claim have an intended or clearly labeled auxiliary purpose?
- Do source binding, configuration, domains, and observations match the claim?
- Are assumptions justified, correctly scoped, and established by callers
  where required?
- Are result correctness, acceptance, errors, and progress covered as promised?
- Do definitions and adapters preserve the distinctions under investigation?
- Are independently useful questions still visible after a bundled refutation?
- Do composition proofs establish their stated architectural purpose?
- Does the registered inventory match the commissioned properties and cover
  the GOAL at its full intended strength?

This review assesses intention, scope, and correspondence. It does not require
the implementation's intended guarantees to be true before proof work begins.

## Callable-contract setup and evidence

The launcher resolves each function heading to exactly one callable in the
unmodified target partition. A function found only in supporting source or
an adaptation-authored shim must not silently become a target binding.

Resolution comes from the callable manifest's structured signature and
reachable effects. A callable inside the supported fragment is
`ExecutableDeferred` and receives `decide`; one outside it is `RoutingOnly`
and receives `prove`, with the unsupported type or effect path recorded.
Failure to build a contract already classified as supported is `build_failed`,
not permission to fall back to `prove`.

For executable counterexample evidence, compare the same callable and witness
in the unadapted and adapted trees. This checks source behavior against
adaptation behavior. It does not establish that the observed behavior violates
the intended contract; that interpretation needs review. The Lean certificate
proves the accepted statement false at the witness.

Record inserted dependency shims and other adaptation changes in the ledger
and disclose them at the advance gate. The missing-import display is upload
help, not an adaptation-routing decision or an acceptance gate.

## Specification changes

Record changes explicitly and map previous requirements to their replacements,
splits, or retirement, with reasons. Retain the original statements, source
and model identities, approvals, proofs, refutations, and run history.

Use a supported specification-revision procedure or create a new campaign.
Preserve pinned documents and their digest checks within an existing run.

Proof reuse requires checking the statement, source binding, definitions,
assumptions, and dependencies in the new context. Revalidate changed claims
and affected dependents. Matching labels alone do not justify transferring
completion status. Faithful models and supporting mathematics may remain
useful even when the goal layer requires substantial revision.
