Measure representative workloads and investigate dominant costs. Use sampling profiles when useful and available; explain limitations and alternatives. Account for setup, reuse, output scope, adverse cases, failures and uncertainty. Competitors and alternative implementations are optional.

Request worthwhile improvements through the implementation loop. Added complexity should plausibly repay its practical cost. Retain the best correct measured candidate; accept when engineering judgment finds no worthwhile remaining improvement. Cite that candidate's measurement IDs in evidence. Acceptance records an empirical engineering judgment about the measured workloads.

Write harness files under benchmarks/harness/ and the assessment in benchmarks/report.md. Return one of:
- {"action":"measure","reason":"workload and purpose","commands":[...]}
- {"action":"improve","reason":"finding","instruction":"requested improvement"}
- {"action":"accept","reason":"selection and stopping judgment","evidence":["measurement ID"],"report":"benchmarks/report.md"}
- {"action":"fail","reason":"blocker"}

Commands have name, argv, cwd, kind (build or run), and optional timeout_seconds. The host measures source/ at /implementation and benchmarks/harness/ at /harness, with writable /build and /output. Set cwd relative to /. Raw observations and executable identities are recorded for each measurement.
