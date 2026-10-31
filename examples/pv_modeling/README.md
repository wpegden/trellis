# Modeling and goal stating

`scripts/pv_modeling.py` prepares source extraction, then launches Modeling in the
production Trellis runtime. Input is a Rust source tree and a collection of goals.

The preparation reviewer directs workers through setup and Rust adaptation.
Each stage ends with an independent audit. Setup records short reproducibility
notes; adaptations explain their rationale in source comments. After adaptation
approval, the source and extraction basis are frozen for the native runtime.
An adaptation audit may hand off diagnosed missing model support after successful
extraction and a full Lean diagnostic check. Workers supply that support as
ordinary paired definition nodes, using `Tablet/model-support.json` for imports
and exact external-declaration bindings. Final completion requires full elaboration.
During adaptation, the reviewer can request an audit-approved return to setup
when extraction configuration needs repair.

Modeling workers state the supplied goals in paired Lean/TeX nodes, adding complete
definitions as appropriate. False goals remain statements for later disproof.
Correspondence, substantiveness, and goal-faithfulness use the production
scheduler, panels, decisions, evidence, invalidation, and blocker handling.
Proofs remain open. With every current gate passing, the reviewer requests the
completion audit. Approval advances the same development into argument development.

Goal declines require audit approval and remain recorded in native state.
An audit-approved source/extraction reopening ends the current runtime through
the standard stop sentinel. For a native viewer creation job, its controller
then re-enters the existing preparation workflow under the same public slug.
It retains the candidate source and approved route, clears preparation approvals,
and obtains fresh setup and adaptation audits before bootstrapping a new runtime.
Earlier preparations, runtimes, and authored artifacts remain read-only references;
no artifact verification or authored tablet is imported. Goal declines retain
their audit history. An irreparable failure remains stopped with its audit reason.
Neither outcome marks the development complete.

Supply a tools JSON file with absolute `cargo`, `charon`, `aeneas`, and `lean`
paths and `lean_paths` containing compatible compiled Aeneas and dependency
libraries. Optional `env`, `readonly`, and `timeout` fields configure the tool
environment, sandbox mounts, and timeout in seconds. Tool installation is an
operator prerequisite. A `profiles` map can supply alternative configurations;
setup selects one with `resource_profile` in `work/extraction.json` and installed
imports with `lean_imports`. Resource contents and actual library resolution
are part of the audited export basis.

`tools.timeouts` can set seconds per operation (`cargo`, `charon`, `aeneas`,
`lean`, `exec`, `compare`, `tool_probe`, `library_resolution`), overriding the
legacy `tools.timeout` default. Extraction and compilation default to 3600 seconds.
Command tools accept `--timeout <seconds>`; `run` and `step` also pass this override
to supervisor checks, agents, and native bootstrap. `TRELLIS_SOURCE_TIMEOUT` supplies
the same run-wide override. Positive finite budgets have no upper clamp.
Budgets are recorded in execution evidence but excluded from model fingerprints;
changing one preserves audits and successful caches and permits retrying a timed-out
supervisor check.

```sh
python3 scripts/pv_modeling.py init /path/to/preparation \
  --source /path/to/rust-crate --goals /path/to/GOAL.json \
  --tools /path/to/tools.json --model gpt-6.1-sol
python3 scripts/pv_modeling.py run /path/to/preparation \
  --repo /path/to/development --runtime /path/to/runtime \
  --prewarm-from /path/to/compatible-existing-repo
python3 scripts/pv_modeling.py show /path/to/preparation
```

The CLI's model selection applies to every runtime role unless `init` receives
`--config-template`. The native viewer template supplies separate
`preparation.worker`, `preparation.reviewer`, and `preparation.audit` bindings
and preserves the selected in-run roles and other launch
configuration through export. Route planning uses the preparation reviewer;
route criticism and route audit use the preparation auditor.
Preparation and main runtime roles default to Sol at xhigh in the native viewer.
Reviews and audits run separately and may use
the same model as the worker.

The viewer's **Independent goals (GOAL.json)** format launches this workflow
from the JSON collection and unmodified source archives. The operator installs
a tools catalog at `<projects-root>/.trellis-viewer/native-tools.json`, or
sets `TRELLIS_NATIVE_TOOLS` on the viewer server. It has the same tools schema
described above and is copied into each creation job. The GOAL author does not
supply tool paths. Preparation is visible under the requested project slug;
the link switches to the native repository after successful handoff. Native
launches keep the existing viewer server running.

If creation stops during preparation, **resume preparation** in the creation
panel continues with the saved source, goals, audits, and launch settings.
The same operation is available as `scripts/pv_create_run.sh retry <slug>`.
An outstanding halt must be investigated and resolved first; retry never
clears halt markers. A failed native bootstrap before runtime initialization
is preserved under `.trellis/modeling/exports/*.failed-*`, including its
partial repository, runtime, and error evidence, before rebuilding the handoff.
Bootstrap installs the repository's checker and runtime snapshot before kernel
initialization. Once a native runtime is initialized, use runtime recovery
controls; preparation retry refuses to replace it. A pending audited reopening
has a durable generation record in the creation job. Its retry finishes the same
handoff, including publication of an already sealed bootstrap. It never replaces
an initialized runtime with another extraction. Halts require their ordinary
resolution, and an operator stop requires explicit Resume. **Resume** on an older
native run that already approved reopening enters this same controller.

Managed native generations live under `native/generations/` in the creation job;
`job.json` identifies the current generation and runtime. The public project link
moves only between that job's predecessor, preparation, and native repository.
Separate runtime paths and process names preserve prior history and allow repeated
reopenings. Unmanaged CLI runs retain their terminal decision and require the
operator to arrange a new preparation.

Runtime installation verifies the kernel's embedded acceptance-source identity
against the installed checker tree and writes `.trellis/runtime/acceptance-build.json`.
When operating from a source checkout, rebuild `trellis_runtime_cli` after changing
kernel, checker, or script sources; deploy the binary and source from that same
checkout. A missing build identity or a source mismatch stops installation.

Frozen extraction declarations must register before native initialization and
before each worker dispatch. This check does not require unfinished Modeling
definitions to compile. A registration failure is a process repair, recorded
before asking an agent to work on files it cannot change. Stopping during
bootstrap terminates its tool sessions and records interrupted partial output
under the same preservation and retry rules described above.

Native Modeling starts with an explicitly uncertified extraction draft, which
may still lack model definitions needed for elaboration. The first accepted
worker submission must pass the ordinary checks and receive complete closure
certificates in that same transaction. Rejected work restores the draft;
the draft cannot complete Modeling or advance to another phase. Creation is
reported as running only after the native runtime reaches its first dispatch.

 Existing Trellis Codex
authentication, tmux, and bubblewrap are required. `--prewarm-from` is optional;
it reuses compatible compiled dependencies. `--prepare-only` stops after the
preparation audits. During preparation, `step` runs one role, `pause` stops after
the current role, and `stop` interrupts it. Once native runtime paths appear in
preparation state, use the ordinary Trellis runtime controls and logs.

Goals have the shape
`{"context": "shared vocabulary", "goals": [{"id": "name", "text": "complete goal"}]}`.
`context` is optional. The complete input is retained in `goals.json` and rendered
in `GOAL.md`.

The preparation directory retains immutable inputs in `upstream/`, candidate
source and extraction configuration in `work/`, observations in `observations/`,
and audit history in `.trellis/modeling/state.json`. Request records and prompts
are in `.trellis/modeling/requests/`; shared execution logs are in
`.trellis/runtime/<preparation-name>-runtime/logs/`. The native repository retains its preparation provenance
in `modeling-input/manifest.json`; authored nodes live in `Tablet/`. Native
protocol state, verifier evidence, prompts, and transcripts use their ordinary
Trellis locations. There is no separate Modeling verifier queue.

`check --through cargo|charon|aeneas|lean` checks the preparation through that
stage. `--fresh` bypasses the stage caches; `--output <writable-directory>` permits
checking a read-only preparation. The native manifest records a fresh check
command. `probe --source <crate>` checks a scratch source copy, and
`inspect --observation <path>` displays bounded evidence with optional symbol,
module, and source filters. Complete observations remain on disk.

`inspect-native <repo>` records exact external-declaration identities;
`compose-native <repo>` regenerates Preamble from the current support nodes and
bindings. `exec <root> --command <argv...>` runs source diagnostics in the
configured environment. Sandboxed `/tmp` uses private disk storage under the run.

The native bootstrap currently accepts one generated module and requires its
extraction to reproduce the audited output. Adapting this bootstrap to split
output remains outstanding.

Phase descriptions are in `trellis/prompt_fragments/pv/process/`. Native roles
receive the long description of their current phase and short descriptions of
the others. Archived standalone Modeling experiments require their saved code.
