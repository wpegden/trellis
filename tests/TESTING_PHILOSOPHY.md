# Testing Philosophy

Tests should test meaning, not duplicate source logic in test logic. They should
be high-level and behavioral wherever possible, and exercise the real project
workflow as fully as practical:

- real repo setup
- real runtime / kernel CLI
- real checker and materialization commands
- real Lean / Lake builds where applicable
- real runtime-issued requests
- real supervisor / runtime / checker orchestration

## 1. Agent-Interaction Tests

Agent-interaction tests cover provider/session behavior that lower-level unit
tests cannot faithfully model:

- provider launch / restart behavior
- session continuity
- fresh vs resume semantics
- stale session collisions
- retries and fallbacks
- auth rotation
- stale-process cleanup
- provider-specific transcript/session discovery
- real provider connectivity / quota / rate-limit behavior

These should be high-level and behavioral. They should test things like:

- does the agent launch?
- does context persist when it should?
- does context reset when it should?
- does retry recover?
- does fallback happen?
- does auth rotation rescue the burst?

They should avoid helper-level syntax assertions unless those are only
supporting checks inside a broader behavioral scenario.

Dedicated agent-interaction tests remain important for cases like:

- real auth rotation under actual account exhaustion
- genuine provider/network outages
- transport corruption or zombie cleanup situations

## 2. Black-Box Bias

The goal is to test meaning, not to duplicate source logic in test logic.

Bad tests:

- restating helper return values
- manually reconstructing internal normalization logic
- mutating kernel/runtime state in ways no real agent could

Good tests:

- a real worker causes an invalid retry
- a real reviewer causes a legal but problematic transition
- a real provider restart loses or preserves context
- a real setup/bootstrap path fails because of permissions or ownership

## 3. Deletion Policy

Delete an old test when one of these is true:

- the same failure mode is now covered by a behavioral scenario through a
  realistic run shape, or
- the old test was checking something no real workflow can actually provoke, so
  we intentionally drop it with no replacement.

We do **not** preserve white-box tests just because they are easy to write.

The final suite should be richer in behavioral coverage and easier to trust,
because failures correspond to real workflow problems rather than test-only
logic mismatches.
