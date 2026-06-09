# Security

**Trellis is only sanctioned for use on a dedicated machine that holds no
private or valuable data.** Please read this before running it.

## Intended use and inherent risk

Driving nondeterministic LLM agents to execute code carries inherent risk:
their behavior is not fully predictable. Trellis has been developed and tuned
for *performance* in hardened, isolated environments that a trusted operator
controls — not yet for wide consumer use. Treat it as operator/research software
and run it only where you accept these risks. It is provided AS IS, without
warranty (see LICENSE).

## Trust model

Trellis drives external LLM agent CLIs (`codex`, `claude`, `gemini`) fully
autonomously, with their approval prompts disabled
(`--dangerously-bypass-approvals-and-sandbox`, `--dangerously-skip-permissions`,
`--approval-mode=yolo`). This is required for unattended multi-cycle operation,
but it means the agents execute commands with **no human in the loop**.

Each worker/reviewer burst runs inside a [`bubblewrap`](https://github.com/containers/bubblewrap)
(`bwrap`) mount sandbox that gives the agent a dedicated burst home and a
read-only view of the project repo (plus a few writable build directories),
drops capabilities (`--cap-drop ALL`), and unshares the pid/ipc/uts namespaces —
so the agent cannot casually read the rest of your home directory.

**That sandbox is containment, not isolation — it is not a hard security
boundary.** Specifically:

- The burst runs as **your own user**. There is no privilege separation.
- It has **full network access** — agents must reach the provider APIs, git, and
  the mathlib cache, so egress cannot be cut.
- It can **read the provider credentials** it needs to authenticate: your
  `~/.codex`, `~/.claude`, and `~/.gemini` are mounted read-only into the worker.
- Unprivileged user namespaces have a non-trivial escape surface.

In short: a buggy, jailbroken, or prompt-injected agent can run arbitrary
commands as you, read your LLM provider credentials, and use the open network to
exfiltrate anything it can reach — the repo, the paper, and those credentials.

## Recommended operating posture

- Run only on a **dedicated or throwaway machine / VM** with **no private or
  valuable data**, no other accounts' secrets, and no production access.
- Use a **dedicated provider account** for the agent credentials — not your
  primary account — and revoke it when you are done.
- Keep `sandbox.enabled: true` (the default). With the sandbox disabled, agents
  run directly on the host with no containment at all.
- Treat the project repo and anything reachable from the host as exposed.

## Reporting a vulnerability

Please report security issues privately to **wes@math.cmu.edu** rather than
opening a public issue.
