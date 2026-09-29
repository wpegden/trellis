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

## The web viewer is an operator control surface

The live viewer (`scripts/start_viewer.sh`, default
<http://127.0.0.1:3301/trellis/>) is not a read-only dashboard. It pauses and
relaunches supervisors, writes operator feedback into a running run, and edits
pause configuration — as your user, un-sandboxed, on a host that holds your
provider credentials. Treat reaching it as equivalent to a shell.

Five layers guard it:

- **It binds loopback.** `TRELLIS_VIEWER_BIND` opts into a wider bind and the
  viewer says so loudly at startup. On this host nginx fronts the viewer on
  443, so loopback is free: you browse by hostname and nginx proxies.
- **It answers only to its own names.** Every request, read or write, must
  carry a `Host` this machine actually goes by. See "DNS rebinding" below.
- **Controls live under a secret path segment.** The viewer mints an
  8-character token (`[A-Za-z0-9]`) and keeps it in
  `<projects root>/.trellis-viewer/control-token` (mode 0600, in a 0700
  directory; the viewer refuses to adopt a token file that is a symlink,
  owned by another user, or readable by others). Browsing
  `…/trellis/<token>/` is the same landing page with the controls on, and
  `…/trellis/<token>/current` is the same run viewer with the controls on.
  Every plain URL keeps working exactly as before, without controls.
- **Every write still needs the `X-Trellis-Control` header.** The path is only
  how the browser learns the token; the header is what authorizes the write,
  and a *custom* header is the point — a browser cannot attach one to a
  cross-origin request without a CORS preflight, and the viewer grants none,
  so a hostile page you visit cannot drive your runs. State-changing endpoints
  also live under `/api/control/`, which both nginx installers deny unless the
  request carries the token prefix.
- **Off loopback, reads need the token too.** See "Token-gated reads" below.

**DNS rebinding: closed, reads included.** A page at `evil.com` that
re-resolves to 127.0.0.1 becomes same-origin with the viewer, and the
same-origin policy stops protecting you. The token always stopped it *driving*
runs — the page cannot guess the secret path segment — but reads were open, so
a malicious page the operator merely visited could pull back papers, chat
transcripts and protocol state from every run on the host.

The viewer now validates `Host` on every request, reads included, because
reads were the exposure. The one thing the attacking page cannot change is the
name in the address bar, so requiring `Host` to name this server ends it. The
allowlist is **derived, never configured**: `localhost`, any `127.0.0.0/8` or
`::1` literal, this machine's own short and fully-qualified names
(`os.hostname()` and `hostname -f`), and whatever `TRELLIS_VIEWER_BIND` names.
Browsing by hostname through nginx therefore keeps working out of the box —
an earlier version of this check demanded the operator configure their own
hostname and was removed for it. `TRELLIS_VIEWER_ALLOWED_HOSTS` adds names for
an alias or a Host-rewriting proxy, and `*` turns the check off; a refusal
names the Host it rejected, the names it accepts, and the variable that would
allow it.

**Token-gated reads.** On a loopback bind — the default — reads stay open:
everything that can reach the port is already on the host. Bound anywhere
else, reads require the same `…/<token>/` prefix the controls use, so a
deliberately exposed viewer does not hand every run's unpublished paper to any
scanner that finds the port. `TRELLIS_VIEWER_READ_TOKEN` overrides in both
directions (`auto` by default, `1` always, `0` never) and the viewer logs which
regime is in force at startup. This is **not** protection against an on-path
observer of plaintext HTTP: the token rides in the URL, so on plain HTTP it is
on the wire along with everything it opens. What it converts is "everything is
public" into "you need the URL". `…/api/health.json` stays open in both
regimes so a process supervisor can probe liveness without holding a secret.

**The tradeoff, deliberately accepted.** Eight characters of `[A-Za-z0-9]` is
about 2^47.6 — ample against remote guessing, especially for a viewer that is
not internet-facing. But the token is now part of the URL, so it appears in
nginx access logs, in browser history, and in anything else that records URLs;
treat those as places the secret lives. Outbound `Referer` leakage is still
covered — the viewer sends `Referrer-Policy: no-referrer` on the pages it
serves, which suppresses the header entirely, and that works the same whether
the token sits in the path or the query string. Rotate by deleting the token
file and restarting the viewer.

What the token does **not** protect, stated plainly:

- **Read access on a loopback bind.** Anything already on the host can read
  every run's state, paper, chat transcripts and event logs without the token.
  Off loopback that is no longer true (see "Token-gated reads"), but the
  **static export** (`scripts/build_public_tablet_viewer.py`) remains the only
  artifact meant for publication.
- **Anyone who can read the token file, a log, or your browser history.**
  `root` and anything running as your user (including a jailbroken agent
  burst, per the trust model above) can read the token and drive the control
  plane; so can anyone with access to the nginx access log.
- **Cross-site scripting in the viewer itself.** The token lives in the page,
  so any script executing in the viewer's origin can use it.

## Deployment modes

**Reach the viewer over an SSH tunnel.** That is the answer. It needs no
account, no new software and no further decisions: anyone running Trellis
already has SSH to the machine.

| Mode | Encryption + authentication | Read surface | Verdict |
| --- | --- | --- | --- |
| **SSH tunnel** | SSH provides both | closed — nothing else can reach the port | **recommended** |
| **TLS with your own certificate** | TLS; token guards writes, and reads only if you gate them | open unless you gate reads or add `auth_basic` / an IP allowlist | viable; decide the read surface explicitly |
| **Plain HTTP** | none | open, and the token is on the wire | **don't** |

```bash
ssh -L 3301:127.0.0.1:3301 <host>   # then browse http://127.0.0.1:3301/trellis/
```

No TLS is needed here: SSH already encrypts and authenticates, and because the
viewer binds loopback the tunnel is the only way in. Two caveats: `-L '*:3301'`
(or `-g`) republishes the viewer in cleartext on the client's own LAN, throwing
away everything the tunnel bought; and the tunnel belongs to the machine rather
than to you, so any local process on the client can open the forwarded port,
and any local process on the host can reach the viewer directly.

Publishing on a public host is a decision about **reads**, not writes. Leaving
`TRELLIS_VIEWER_READ_TOKEN` at `auto` already gates them behind the token URL
once the bind is not loopback; behind a proxy that fronts a loopback viewer,
set it to `1` explicitly, since the viewer cannot see that it is exposed. For
a stronger read surface than "you need the URL", add `auth_basic` or an IP
allowlist — both nginx installers take `AUTH_FILE=` and `ALLOW_FROM=` for
exactly this — or publish the static export instead. Over plain HTTP the token
rides in the URL and therefore on the wire, so read exposure becomes control
exposure; the installers refuse to generate that configuration unless you set
`ACCEPT_PLAINTEXT_EXPOSURE=1`.

## Recommended operating posture

- Run only on a **dedicated or throwaway machine / VM** with **no private or
  valuable data**, no other accounts' secrets, and no production access.
- Use a **dedicated provider account** for the agent credentials — not your
  primary account — and revoke it when you are done.
- Keep `sandbox.enabled: true` (the default). With the sandbox disabled, agents
  run directly on the host with no containment at all.
- Treat the project repo and anything reachable from the host as exposed.
- Leave the viewer on loopback and reach it over a tunnel rather than widening
  the bind (see "Deployment modes" above).

## Reporting a vulnerability

Please report security issues privately to **wes@math.cmu.edu** rather than
opening a public issue.
