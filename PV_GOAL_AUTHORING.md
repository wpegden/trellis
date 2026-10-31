# Writing a program-verification GOAL document

A GOAL specifies the properties a verification campaign investigates. It
contains the subject, shared definitions and assumptions, and declarative
goal statements. The process uses it to establish the intended meaning of
formalized claims. Trellis handles the workflow and agent instructions.

## Native modeling format

In the viewer, select **Independent goals (GOAL.json)**. This format accepts
a JSON collection with this structure:

```json
{
  "context": "The subject is select_larger in the supplied source.",
  "goals": [
    {
      "id": "maximum",
      "text": "For every normal return from select_larger, the result equals the maximum of its inputs."
    },
    {
      "id": "normal_return",
      "text": "For every input, select_larger terminates normally without panicking."
    }
  ]
}
```

`context` is optional shared prose. It supplies scope, definitions, and
assumptions needed to interpret the statements. Each `goals` entry has a
unique, nonempty `id` and complete, nonempty `text`. Choose short, descriptive
IDs using lowercase letters, digits, and underscores, starting with a letter.
Put each property that needs its own result in a separate entry. Use `context`
for shared meaning and assumptions, rather than additional claims to verify.

Author the JSON collection. Trellis generates a readable `GOAL.md` with the
context followed by one `## <id>` section per goal; you do not need to maintain
a second document. Each entry becomes a separate verification target. Several
goals may concern the same function. Mathematical specification properties
may also have their own entries.

Name the subject of each goal and state its property under the shared
context. Trellis uses the source upload and launch settings to resolve
functions and record the execution configuration.

## Statement contents

A statement should identify:

- The program behavior or mathematical property being claimed.
- Any domain restriction or assumption needed to interpret that claim.
- The intended result, relation, or state effect.

Ordinary language and references to available contracts are sufficient. Source
signatures and conventional mathematical definitions can be developed by the
process. Explain application-specific facts, unusual meanings, and deliberate
departures from a referenced contract.

A documented upstream guarantee, an explicit owner requirement, and a
justified helper contract are all valid subjects. The implementation may
violate them. Label diagnostic implementation properties and mathematical
support properties according to their roles.

Keep claims about normal results distinct from termination requirements.
For example, the two entries above ask separate questions about value and
termination. A single conjunction receives one result: a counterexample to
one part does not settle the others.

You do not need to supply formal definitions, proofs, or a proof plan. Trellis
develops the formal specification and handles its review. Include any
referenced contract text that is needed to understand the goals; the run
cannot fetch references from the network.

## Callable-contract launcher format

The upload launcher described in [PV_PHASE0_RUNBOOK.md](PV_PHASE0_RUNBOOK.md)
also supports **Function contracts (GOAL.md)**. This format declares a callable contract
with a level-three heading containing the function name in backticks:

```markdown
# GOAL

### `select_larger`

For every input, the function terminates normally without panicking and
returns the maximum of its inputs.
```

Each heading registers one callable target with one contract statement.
Multiple clauses under a heading remain part of that contract. This format
does not give those clauses separate target identities.

Use a unique ASCII Rust leaf identifier from the target upload, without a
module path, type parameters, arguments, or a descriptive suffix. Target
headings must be outside lists, quotes, comments, and code fences. Names must
be mutually prefix-free: `parse` and `parse_number` cannot both be targets.
Omit `[decide]`/`[prove]` suffixes; Trellis selects the verification method.

An optional syntax check for this format is:

```sh
python3 scripts/pv_prose_goal.py path/to/GOAL.md
```

This checks callable headings, not the native JSON collection or its Markdown
rendering. Select the format required by the chosen intake route.

See [PV_SEED_AUTHORING.md](PV_SEED_AUTHORING.md) for source inputs and trust
boundaries. [PV_GOAL_REVIEW.md](PV_GOAL_REVIEW.md) describes how Trellis
develops and reviews the resulting specification.
