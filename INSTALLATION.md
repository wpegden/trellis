# Installation

End-to-end setup for a trellis host. Trellis is a long-running supervisor that
drives external agent CLIs against a Lean formalization repo; the install has
three layers:

1. **System prerequisites** — packages, bwrap, kernel namespaces
2. **Per-user setup** — repo clone, AI CLI auth, viewer, optional Loogle
3. **Build + smoke** — kernel binary and tests

**Recommended platform: Ubuntu 26.04 LTS** — Trellis is validated end-to-end
on it with no root setup at all (the sandbox works on stock policy; see
"Choosing a distro"). Ubuntu 24.04 works too but needs the one-time root
sysctl in §1. Any Linux with `bwrap` available, a recent Rust toolchain,
Python 3.12+, and Node 22.x should work.

> ⚠️ **Security — read [SECURITY.md](SECURITY.md) first.** Trellis runs agent
> CLIs fully autonomously with approvals disabled. The `bwrap` sandbox is
> containment, not a hard boundary: bursts run as your own user, with network
> access and read access to your provider credentials. **Only install and run
> Trellis on a dedicated machine with no private or valuable data.**

> **No Python *package* dependencies (for the supervisor + tests).** The
> supervisor and the test suite run straight from the repo using only the Python
> 3.12 standard library — there is no `pip install` step and no
> `requirements.txt`. Run it with the repo on `PYTHONPATH`
> (`PYTHONPATH=~/src/trellis python3 -m pytest ...`); the operator scripts set
> this for you. The one exception is the **optional** terminal monitor
> (`python -m trellis.cli_monitor`), which imports `rich` — install it with
> `pip install rich` only if you want that tool (see §4 in `README.md`). Note,
> however, that the **test suite is not dependency-free** in the toolchain
> sense: it requires the built kernel binary (see §3 — build it *before* running
> `pytest`), and the full suite additionally requires a Lean / `lake` install
> (see §2). Running `pytest` without the kernel binary fails with
> `cannot find cargo for trellis kernel invocation`.

## System requirements

- **RAM: 32 GB minimum, 48 GB recommended.** The peak is real, not headroom:
  mathlib compilation and single-node Lean elaboration can each claim tens of
  GB, and a run stacks them on top of the supervisor, the checker server, and
  several concurrent agent CLIs. Below 32 GB expect OOM kills mid-burst.
- **SSD strongly recommended.** Runs are disk-churn heavy — mathlib caches,
  per-grunt workspace clones with olean copies, and continuous git checkpoint
  traffic. Budget ~100 GB free for toolchains, caches, and a run's workspaces,
  and put swap on the SSD too, with room to spare.
- **The closure sidecar ("grunts") is optional and off by default**, and it
  raises every requirement above — each grunt adds a workspace clone and a
  concurrent Lean build. It buys a modest speedup and some token savings, not
  a better chance of formalizing anything. Leave it off unless the numbers
  above are comfortably exceeded (README §10).
- **Linux.** The sandbox is Linux-specific (`bwrap`, user namespaces) and the
  supervisor leans on `/proc` and tmux process semantics. Distros differ on
  whether stock policy lets an unprivileged user run the sandbox — see
  "Choosing a distro" below; macOS is a VM host, not a target (see below).

### Choosing a distro

Trellis's sandbox needs unprivileged user namespaces, and distros differ on
whether stock security policy allows them. With root available, any modern
Linux works (Ubuntu 23.10/24.04 need the one-time §1 sysctl). If you will
**not** have root on the machine, choose the distro accordingly:

**Sandbox works on stock policy — no sudo at any point** (provided `bwrap` is
installed, which flatpak hosts already have):

- **Ubuntu 26.04 LTS** (and 25.04+) — **the recommended default**, validated
  end-to-end with zero root setup. The userns restriction is still on, but
  these releases ship a `bwrap-userns-restrict` AppArmor profile that
  whitelists the **packaged** `/usr/bin/bwrap`. Use the distro package: a
  self-built `bwrap` is blocked by the same restriction — the inversion of the
  build-your-own advice below.
- **Ubuntu 22.04 LTS** — predates the restriction entirely.
- **Linux Mint 22.x and Pop!_OS 24.04** — both deliberately undo Ubuntu's
  restriction (Mint ships a sysctl override; Pop's kernel omits the patch).
- **Debian 12+, Fedora, RHEL 8/9/10 and derivatives (Rocky/Alma/CentOS
  Stream), openSUSE, NixOS, Arch** (stock kernels — `linux-hardened` blocks
  unprivileged userns, and DISA-STIG/OSPP-hardened RHEL images set
  `user.max_user_namespaces=0`). On all of these, `bwrap` needs no privileges
  and can even be built from source into `~/.local/bin` without root if the
  package is missing.
- **WSL2** — Microsoft's kernel carries no userns restriction regardless of
  the Ubuntu userland inside. Keep the Trellis repo and runtime in the ext4
  rootfs, not under `/mnt/c`.

**Blocked without a one-time root step — Ubuntu 23.10, 24.04 LTS, and
24.10.** These ship the userns restriction with no bwrap profile (one was
added in a 24.04 point release, then reverted from both 24.04 and 24.10; it
returned, enabled, in 25.04), so there is no unprivileged way through — an
unpackaged `bwrap` build is restricted the same way. On these releases: apply
the §1 sysctl (root, once), ask an admin for it (a small, standard ask), pick
a release from the list above, or run inside a VM.

**No `bwrap` at all — macOS, Windows.** VM territory (see the macOS section).

To probe any machine, as an ordinary user:

```bash
bwrap --unshare-user --ro-bind / / true && echo sandbox-ok
```

If it fails, `sysctl kernel.apparmor_restrict_unprivileged_userns` tells you
whether you are on the blocked island (`= 1` with no working packaged bwrap)
or just missing the package. Probe with `bwrap`, not `unshare` — on the
profile-based Ubuntu releases `unshare -r` fails while the packaged bwrap
works.

### Root, and running without it

Trellis itself never runs as root — after the one-time §1 host setup,
everything (supervisor, agents, sandbox) runs as your own user. Standard
installation needs root **once per host**, for two things: installing packages
(`bubblewrap` and friends), and — on Ubuntu 23.10/24.04 only — the §1 AppArmor
sysctl. There is deliberately no unsandboxed mode to fall back on: bursts run
agent CLIs with approvals disabled, and the sandbox is a hard prerequisite,
not an optimization. With no root at all, the distro choice above is the
answer; the VM below is the fallback when the machine's OS is not yours to
choose.

**VM path, and its cost.** A KVM-accelerated VM (virt-manager/libvirt, QEMU
with `-accel kvm`, virtio disk) runs Lean builds at near-native CPU speed —
the performance hit that matters is I/O, so give the guest ≥32 GB RAM and keep
its disk image on SSD. KVM needs `/dev/kvm` access (granted automatically to
a logged-in desktop session on most distros; from ssh it usually means `kvm`
group membership — an admin ask). Without `/dev/kvm`, QEMU falls back to pure
emulation, which is not practical for mathlib builds. Inside the guest you are
root, so §1 applies normally — or install a guest release from the
no-sudo list and skip even that.

### macOS

Run Trellis in a Linux VM. On Apple Silicon, UTM or Lima with the Apple
Virtualization framework and an arm64 Ubuntu 26.04 guest works well — every
toolchain in §1–2 ships arm64 builds — at near-native speed. Allocate ≥32 GB
of RAM to the guest and keep the VM disk on the internal SSD. Inside the guest
you are root, so the standard §1 setup applies. Docker Desktop / OrbStack
containers also wrap a Linux VM, but `bwrap` nested inside a container needs
extra privileges the defaults deny — a full VM is simpler and matches the
dedicated-machine security posture anyway.

## Quickstart

The end-to-end path a fresh host follows, as an orientation map. Each step links
to a detailed section below — consult those for flags, env vars, and gotchas.

```bash
# 1. System prerequisites (§1) — root, once per host. On Ubuntu 26.04/25.04+
#    with the packages already present, there is nothing to do here at all.
apt install git python3 tmux curl bubblewrap build-essential   # + Node 22.x
sysctl -w kernel.unprivileged_userns_clone=1                    # if disabled by default
sysctl -w kernel.apparmor_restrict_unprivileged_userns=0       # Ubuntu 23.10–24.10 only — 25.04+/26.04 ship a bwrap AppArmor profile instead

# 2. Per-user setup (§2) — as the operator.
git clone https://github.com/<your-org>/<your-repo>.git ~/src/trellis
npm install -g @google/gemini-cli @anthropic-ai/claude-code @openai/codex
gemini; claude; codex                                          # authenticate each, once
curl https://raw.githubusercontent.com/leanprover/elan/master/elan-init.sh -sSf | sh -s -- -y --default-toolchain none
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

# 3. Build the kernel (§3).
cd ~/src/trellis
cargo build --bin trellis_runtime_cli --manifest-path kernel/Cargo.toml

# 4. Create a project repo (§4). --loogle on|off is required.
./scripts/setup_repo.sh --loogle off <repo_path> <paper_tex_path> [project_slug]

# 5. Preflight the providers (§4) — cheap, before a real run.
python3 -m trellis.provider_check --config <repo_path>/trellis.config.json

# 6. Launch (§5).
./scripts/restart_configured_run.sh <repo_path>/trellis.config.json <runtime_root>
```

## 1. System prerequisites (run as root, once per host)

On the recommended platform (Ubuntu 26.04 LTS) this section reduces to
"install the packages" — and to nothing at all if they are already present;
the userns sysctls below apply only to the blocked releases named there.

Install at the system level:

- `git`, `python3` (3.12+), `tmux`, `curl`
- `python3-pip` (only for the optional `cli_monitor` terminal UI — it needs
  `pip install rich`; the supervisor and tests do not)
- `bubblewrap` (`bwrap`) for worker sandboxing
- A C/C++ toolchain (`build-essential` or equivalent) for Rust + Lean toolchain
  builds
- Node 22.x (via nvm or system package) for the viewer
- `nginx` is only needed if you intend to expose the viewer publicly

bwrap relies on Linux user namespaces. On distros that disable unprivileged
user namespaces by default, enable them with:

```bash
sysctl -w kernel.unprivileged_userns_clone=1
echo 'kernel.unprivileged_userns_clone=1' > /etc/sysctl.d/99-userns.conf
```

On Ubuntu 23.10 through 24.10 the AppArmor `userns` restriction is the more
likely blocker: even with `unprivileged_userns_clone=1` set, `setup_repo.sh`
aborts with `bwrap: setting up uid map: Permission denied` because
`kernel.apparmor_restrict_unprivileged_userns=1` is on by default and those
releases ship no bwrap AppArmor profile. (On 25.04+ — including 26.04 LTS —
the shipped `bwrap-userns-restrict` profile whitelists the packaged
`/usr/bin/bwrap`, so this sysctl is unnecessary there — see "Choosing a
distro".) On the blocked releases, disable it:

```bash
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
echo 'kernel.apparmor_restrict_unprivileged_userns=0' | sudo tee -a /etc/sysctl.d/99-userns.conf
```

### Worker sandbox

Trellis runs every worker/reviewer burst inside a `bubblewrap` (`bwrap`) mount
sandbox: the agent gets a dedicated burst home and a read-only view of the
project repo (plus a few writable build paths) and cannot see the rest of your
home directory. The burst runs as **your own user** — there is no separate
sandbox account or sudo split (the earlier dedicated-user model was retired in
the bwrap-only migration). The sandbox is containment, not a hard boundary (see
the banner above and [SECURITY.md](SECURITY.md)).

Just confirm `bwrap` is installed:

```bash
which bwrap    # /usr/bin/bwrap
```

## 2. Per-user setup (run as the operator)

### 2a. Clone the repo

```bash
git clone https://github.com/<your-org>/<your-repo>.git ~/src/trellis
```

### 2b. AI CLIs

The supervisor invokes one or more of `gemini`, `claude`, and `codex` as
subprocesses (which ones are required depends on your `trellis.config.json`).
Install them user-local (NOT system-wide):

```bash
mkdir -p ~/.local/share/npm-global
npm config set prefix ~/.local/share/npm-global
echo 'export PATH=~/.local/share/npm-global/bin:$PATH' >> ~/.profile
source ~/.profile
npm install -g @google/gemini-cli @anthropic-ai/claude-code @openai/codex
```

Authenticate each one interactively, once — **as the same operator account that
runs trellis**, so the tokens land in that account's home. The worker sandbox
bind-mounts these exact credential directories read-only into the burst (it
binds `~/.codex`, `~/.claude`, and `~/.gemini` from the operator's home — see
`trellis/sandbox.py`). If you authenticate as some other user, the burst won't
see the tokens and every burst fails. Each CLI writes the file the sandbox
relies on:

- `gemini` → completes the OAuth flow, writes `~/.gemini/oauth_creds.json`
- `claude` → writes `~/.claude/.credentials.json`
- `codex` → writes `~/.codex/auth.json`

> **Gemini OAuth env vars (optional — quota display only).** The `gemini`
> provider authenticates from `~/.gemini/oauth_creds.json` (bound read-only into
> the burst), so it does **not** need any extra env vars to run. The
> `GEMINI_OAUTH_CLIENT_ID` / `GEMINI_OAUTH_CLIENT_SECRET` vars are read only by
> the best-effort quota-display panel (`trellis/gemini_quota_api.py`); set them
> if you want that panel populated, otherwise leave them unset. Note also that
> `setup_repo.sh`'s gemini validation runs the CLI with
> `GEMINI_CLI_TRUST_WORKSPACE=true` to skip gemini's interactive
> trusted-directory dialog (production bursts handle this internally via a seeded
> `~/.gemini/trustedFolders.json`).

### 2c. Lean toolchain (elan)

```bash
curl https://raw.githubusercontent.com/leanprover/elan/master/elan-init.sh -sSf | sh -s -- -y --default-toolchain none
source ~/.elan/env
```

Or install via your distro's package manager if it ships `elan`.

> **Toolchain + mathlib pin.** `setup_repo.sh` pins each project repo to a
> specific Lean toolchain (`leanprover/lean4:v4.33.0`) and a specific
> mathlib revision (the `MATHLIB_REV` default in `scripts/setup_repo.sh`,
> currently the `v4.33.0` tag `db584cd6d46c92f209a44c0f1c829460d327499d`);
> override either with the `MATHLIB_TOOLCHAIN` / `MATHLIB_REV` env vars. On the
> first project build, setup runs `lake exe cache get` to download mathlib's
> prebuilt `.olean` cache (a few-GB download, usually minutes) rather than
> building mathlib from source — only your project's `Tablet` compiles locally.

### 2d. Rust toolchain

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
```

### 2e. Viewer (set this up — it is the primary operator interface)

The viewer is a Node service that reads supervisor state from a projects-root
directory and serves an interactive DAG and progress UI. It is how you are
expected to interact with a formalization as it runs — approval gates and
`NeedInput` escalations surface there with approve/send-input as the primary
actions, alongside halt and pause banners, budget controls, and the grunt
pool. A run without the viewer is operable from the CLI, but gates are much
easier to miss. Build dependencies and start it via the wrapper script:

```bash
cd ~/src/trellis/viewer
npm install
bash ../scripts/start_viewer.sh
```

By default `start_viewer.sh` launches the viewer in a dedicated tmux session
on `127.0.0.1:3301`. Override via `TRELLIS_VIEWER_PORT`,
`TRELLIS_PROJECTS_ROOT`, `TRELLIS_VIEWER_BASE_PATH` (see the script for the
full list).

The viewer binds `127.0.0.1` and will not answer from the network unless you
set `TRELLIS_VIEWER_BIND` (it warns loudly when you do). Its state-changing
endpoints require a secret header and appear only under a secret path segment;
set `TRELLIS_VIEWER_CONTROL=0` to make it read-only before exposing it
anywhere. See SECURITY.md.

It also answers only to its own names — `localhost`, loopback literals,
`os.hostname()`, the fully-qualified name from `hostname -f`, and whatever it
bound — which blocks DNS rebinding without needing configuration. If you reach
it under an alias or through a proxy that rewrites `Host`, add that name to
`TRELLIS_VIEWER_ALLOWED_HOSTS` (comma-separated; `*` disables the check); a
refused request prints exactly this in its response body. And once the bind is
not loopback, reads require the token path too — `TRELLIS_VIEWER_READ_TOKEN`
is `auto` by default, `1` forces it on (do this when a proxy fronts a
loopback-bound viewer, which the viewer cannot detect), `0` off.

> **Enabling the controls.** Viewing works at the plain URL; pause/resume and
> feedback appear only under a secret 8-character path segment.
> `start_viewer.sh` prints a `viewer_control_url=` line — browse
> `…/trellis/<token>/` and everything below it behaves exactly like the plain
> viewer with the controls switched on (`…/trellis/<token>/current` is the
> `current` run, and so on). The token is in `$HOME/.trellis-viewer.log` and at
> `<projects root>/.trellis-viewer/control-token`; delete that file and restart
> to rotate it.
>
> Behind nginx, the control subtree must be **proxied to the viewer, not served
> from the static export** — the exported HTML has no token in it. Both
> installer scripts emit the rule; an existing hand-written site needs
> `location ~* "^/trellis/[A-Za-z0-9]{8}(/|$)"` proxying to the viewer port,
> placed before any `/api/control/` deny rule.

> **Viewer URL + project discovery.** The viewer serves under its `BASE_PATH`
> (default `/trellis`), so the working URL is
> <http://127.0.0.1:3301/trellis/> — a bare <http://localhost:3301/> returns
> "Cannot GET /". That URL is a landing page listing every run on the host
> with its lifecycle state; each card links into that run's viewer, and
> `?goto=default` still jumps straight to the default project.
> Discovery treats each entry of `TRELLIS_PROJECTS_ROOT`
> (default `~/math`) as a project repo directly, so the repo must appear *as*
> `~/math/<name>`. The launcher does this by pointing `~/math/current` at the
> repo; if you run with `--no-current` or drive the CLI by hand, add the symlink
> yourself: `ln -sfn <repo_path> ~/math/current`.

### 2f. Loogle (optional)

The local Loogle (Mathlib search) endpoint at `http://127.0.0.1:8088/` is
optional and gated per project by the `loogle.enabled` knob in
`trellis.config.json`. `setup_repo.sh` **requires** an explicit `--loogle on|off`
flag (§4) and writes that value into the generated config; when the `loogle` key
is absent entirely the supervisor defaults to ON, but freshly-generated projects
always carry an explicit value. With Loogle on, run a Loogle server — it is
independent of trellis; see <https://github.com/nomeata/loogle> for upstream
install instructions.

When Loogle is off, the worker prompt fragment about Loogle is omitted
automatically. The worker skill files `skills/THEOREM_STATING_WORKER.md` and
`skills/PROOF_FORMALIZATION_WORKER.md`, however, still carry a "Loogle First"
section; if you are not running a Loogle server, edit those two files to remove
the Loogle guidance.

### 2g. Isabelle/HOL backend (optional)

Trellis can formalize into Isabelle/HOL instead of Lean. Skip this section
entirely for Lean runs — a repo with no `workflow.default_target` key always
resolves to Lean.

- **Distribution.** Install **Isabelle2025-2 or newer** from
  <https://isabelle.in.tum.de/>. The requirement is enforced at session start:
  2025-1 admitted `sorry` in ways the soundness gate assumes impossible, so an
  older distribution is refused rather than warned about.
- **Location.** The sandbox binds the distribution from
  `TRELLIS_ISABELLE_HOME` (default `~/Isabelle2025-2`), and the checker and
  kernel resolve the launcher from `TRELLIS_ISABELLE_BIN` (default
  `~/Isabelle2025-2/bin/isabelle`). The two defaults are computed
  independently, so if you unpacked Isabelle anywhere else, export both in
  every shell that runs setup, the checker server, or the supervisor:

  ```bash
  export TRELLIS_ISABELLE_HOME="$HOME/Isabelle2025-2"
  export TRELLIS_ISABELLE_BIN="$TRELLIS_ISABELLE_HOME/bin/isabelle"
  ```

- **Project creation.** Use the shipped Isabelle templates —
  `examples/trellis.isabelle.config.json` and
  `examples/trellis.isabelle.policy.json` (the policy sibling is picked up
  automatically) — by passing the config as the `CONFIG_TEMPLATE` (see the
  config-template note in §4):

  ```bash
  CONFIG_TEMPLATE=examples/trellis.isabelle.config.json \
    ./scripts/setup_repo.sh --loogle off <repo_path> <paper_tex_path> [project_slug]
  ```

  The template sets `workflow.default_target: "isabelle_hol"`, which is the
  single switch the kernel and Python route on. Setup then prebuilds a
  `Tablet_Base` warm session (parented on HOL-Probability) into shared system
  heaps, binds the distribution read-only into the worker sandbox, and
  installs `FILESPEC_isabelle.md` as the backend's file-shape contract (the
  analogue of `FILESPEC.md`).
- **What runs differently.** A warm checker session keeps the accepted prefix
  elaborated so a re-check costs only the in-flight theory, cross-checked
  against a cold rebuild on a cadence (any warm-vs-cold disagreement halts the
  run); `quick_and_dirty=false` is the enforced build floor; workers get an
  `isa-query` discovery helper (`find_theorems` / `find_consts` /
  `solve_direct` / `sledgehammer` / `thm_oracles`) emitted into the run repo.
  Loogle is a Mathlib-only affordance, so `--loogle off` is the right choice
  for Isabelle runs.

## 3. Build the kernel binary and run tests

```bash
cd ~/src/trellis
cargo build --bin trellis_runtime_cli --manifest-path kernel/Cargo.toml
cargo test --manifest-path kernel/Cargo.toml --lib
python3 -m pytest tests/ -q
```

Build the kernel binary **before** running `pytest`: the Python test suite
invokes it, and without it ~16 tests fail with
`cannot find cargo for trellis kernel invocation`.

The supervisor runs the debug binary at
`kernel/target/debug/trellis_runtime_cli`. A `--release` build works but is
not required.

Some tests are environment-gated rather than broken — distinguish "expected
skip/fail on a bare host" from a real regression:

- The Lean-dependent tests (`lean_semantic_*`, `print_axioms`) require a
  working Lean / `lake` install (§2); they will fail or skip on a host without
  it.

A fully green run requires the kernel binary built **and** a Lean/`lake`
toolchain present.

## 4. Setting up a project repo

Trellis does NOT ship a Lean project. You bring your own paper (`.tex`) and
Lean scaffold. Give `<repo_path>` its own directory **outside the trellis
source tree** — conventionally under the projects root the viewer reads
(default `~/math`), e.g. `~/math/connectivity`. Any location works; point the
viewer at it with `TRELLIS_PROJECTS_ROOT`. Create the repo with:

```bash
./scripts/setup_repo.sh --loogle on|off <repo_path> <paper_tex_path> [project_slug]
```

`--loogle on|off` is **required**; it sets `loogle.enabled` in the generated
`trellis.config.json` (see §2f). For a clean rebuild in place:

```bash
./scripts/setup_repo.sh --reset --yes --loogle on|off <repo_path> <paper_tex_path> [project_slug]
```

> **`BURST_PATH` (lake/elan only).** `setup_repo.sh`'s Lean prewarm runs `lake`
> under `BURST_PATH`, which defaults to `$HOME/.elan/bin:/usr/local/bin:/usr/bin:/bin`
> (the script appends `$HOME/.elan/bin` so §2c's elan install resolves out of the
> box). The **provider CLIs no longer need to be on `BURST_PATH`**: the worker
> sandbox PATH (`host_runtime.worker_path_env`) now resolves `codex`/`claude`/
> `gemini` from wherever §2b installed them (user-local npm-global, nvm, or
> `/usr/local/bin`) — the same dirs the sandbox bind-mounts read-only — and
> setup validates the providers under that real burst PATH. If `lake` lives
> somewhere unusual, still extend `BURST_PATH` to cover it:
>
> ```bash
> export BURST_PATH="$HOME/.elan/bin:/usr/local/bin:/usr/bin:/bin"
> ```

This produces a self-contained repo with `Tablet/`, the Lean toolchain
configuration, and the `.trellis/` runtime hooks. The supervisor will then
operate against `<repo_path>` and its sibling runtime root.

> **Config template.** A default config/policy ships at
> `examples/trellis.config.json` and `examples/trellis.policy.json` (the `codex`
> provider for all roles). `setup_repo.sh` uses these as the template by
> default, rewriting the project-specific fields (repo path, paper path,
> main-result targets, session name) into the generated
> `<repo_path>/trellis.config.json`. Point it at a different template with
> `CONFIG_TEMPLATE=/path/to/your.config.json` (a sibling `*.policy.json` is
> picked up automatically).

> **Revising an existing tablet.** To update an already-formalized tablet
> against a newer paper version instead of starting fresh, use
> `./scripts/setup_revision_repo.sh` (`--base-repo`, `--old-paper`,
> `--new-paper`, `--out-repo`) in place of `setup_repo.sh`. It prepares the
> working repo, points `workflow.paper_tex_path` at the new source plus a
> `workflow.revision` block, and seeds a `RevisionStating` state that inherits
> the prior approvals and reopens only the changed delta. Then launch the normal
> run (§5) against the prepared repo. See README §9 ("Revision Runs") for the
> full workflow.

### Provider / model / effort per lane

The shipped `examples/trellis.config.json` runs `codex` for every role, but each
lane is configured independently. A lane is an object with `provider`, `model`,
and `effort`:

- `worker`, `easy_worker`, `hard_worker` — the proof-writing lanes
- `reviewer` — the cycle-advancing reviewer
- `stuck_math_audit` — the stuck-math audit lane. Absent from a config it
  falls back to `reviewer`; the shipped templates now set it explicitly, so
  changing `reviewer` alone no longer moves it.
- `verification.correspondence_agents`, `verification.soundness_agents`,
  `verification.substantiveness_agents` — the agent-verified check lanes (each a
  list of lane objects, so a check can be voted by more than one agent)

You can mix providers across lanes (e.g. a `codex` worker with a `gemini`
reviewer) by setting each lane's `provider`/`model`/`effort` to taste.

**How `effort` is passed (verified in the agent backends):**

- `codex` — passed as `reasoning_effort` (e.g. `xhigh`) via the headless
  `-c reasoning_effort=…` flag (`trellis/agents/codex_headless.py`).
- `claude` — passed as `--effort` on the CLI launch
  (`trellis/agents/tmux_backend.py`).
- `gemini` — the tmux launch passes only `--model`; `effort` is **ignored**
  (`trellis/agents/tmux_backend.py`).

Model strings age out as providers release new models. The shipped example pins
a specific `codex` model in the `model` field; to update, set each lane's `model`
to a model string your installed CLI accepts (check the provider's CLI/docs for
the current identifier). Whatever you change, **commit it** — see the run-loop
footgun in `README.md` (the run loop `git reset --hard`s the repo, reverting any
uncommitted edit to `trellis.config.json`).

### Preflight your providers

Before a real run, validate that the providers your config is set to use
actually work — cheaply, up front:

```bash
python3 -m trellis.provider_check --config <repo_path>/trellis.config.json
```

Layer 1 (no API) checks the bwrap sandbox, that `lake`/`lean` and every
configured provider CLI resolve on the worker PATH, and repo-root write
protection. Layer 2 fires one real one-shot burst per distinct provider/model
and confirms it emits structured-output JSON that round-trips — catching
auth-not-done, bad model strings, and headless-agent issues that otherwise only
surface mid-run. Useful flags: `--sandbox-only` (skip the API bursts),
`--lean-mathlib` (deepen the lean probe to import Mathlib), and
`--lanes worker,reviewer` (restrict to specific lanes).

## 5. Launch a run

> **Launch from a shell where every CLI resolves.** The supervisor builds the
> worker burst's `PATH` and the read-only CLI/elan binds by calling
> `shutil.which` on its **own** environment at burst time
> (`trellis/host_runtime.py`: `worker_provider_bin_dirs` / `worker_elan_home` /
> `worker_path_env`). So you must launch the run from a shell where every tool
> resolves — e.g. nvm sourced, elan on `PATH`. This is the single most likely
> silent failure for a new host. Confirm first:
>
> ```bash
> which codex claude gemini lake node    # all must succeed
> ```

The supported one-command launcher does everything below in the right order:

```bash
./scripts/restart_configured_run.sh <config_path> <runtime_root>
```

`<config_path>` is the generated `<repo_path>/trellis.config.json` (it carries
`repo_path`); `<runtime_root>` is where runtime state lives. The launcher
recreates the repo with `setup_repo.sh --reset`, reinitializes the runtime with
`trellis.sh init`, starts the viewer, starts the unified-checker server in a
sibling `trellis-checker-<slug>` tmux session, waits for it to bind its UNIX
socket, exports `TRELLIS_CHECKER_SOCKET`, and runs the supervisor
(`trellis.sh run`) in a `trellis-run-<slug>` tmux session with that socket in
its environment. Watch either session with `tmux -L trellis attach -t
<session>`. Use `--no-run` to set up without launching, or `--check-only` for a
dry run.

> **Runtime-root form.** The checker server derives the repo from
> `<runtime_root>` and only accepts the inner form
> `<repo>/.trellis/runtime/<name>` or the outer (sibling) form
> `<parent>/<repo_basename>-runtime`. Use one of these; an arbitrary path that
> `trellis.sh init` would otherwise accept will be rejected by the checker
> server.

## 6. Manual run (what the launcher does under the hood)

If you drive the CLI by hand instead of using the launcher, you must start the
checker server and export its socket before running the supervisor — every
acceptance lake check routes through the server (there is no host-lake
fallback), so omitting the socket fails at the first check.

```bash
# 1. Initialize the runtime.
./scripts/trellis.sh init <config_path> <runtime_root>

# 2. Start the checker server against the SAME runtime_root (NOT the repo
#    path); it binds <runtime_root>/sockets/checker.sock. Leave it running.
#    Launch it (like the supervisor, §5) from a shell where your toolchain
#    resolves — it runs `lake` for acceptance checks.
bash scripts/trellis_checker_server.sh <runtime_root>

# 3. Export the socket and run the supervisor.
export TRELLIS_CHECKER_SOCKET=<runtime_root>/sockets/checker.sock
./scripts/trellis.sh show <runtime_root>
./scripts/trellis.sh run  <runtime_root>
```

For bounded execution (single step / N steps), with the socket still exported:

```bash
./scripts/trellis.sh step <runtime_root>
./scripts/trellis.sh run  <runtime_root> 1
```

See `README.md` for runtime operation details (preview, step, graceful stop,
inspecting `protocol_state.json` and `event_log.jsonl`).

## Verification checklist

```bash
# Sandbox (bursts run as you, inside bwrap — no separate sandbox account)
which bwrap                       # /usr/bin/bwrap
bwrap --ro-bind / / true          # exits 0 if user namespaces work

# AI CLIs (only those you intend to use)
which gemini claude codex

# Toolchains
lean --version; elan --version; cargo --version; node --version

# Viewer (if installed) — note the /trellis/ base path; bare / returns "Cannot GET /"
curl -s http://127.0.0.1:3301/trellis/ -o /dev/null -w '%{http_code}\n'   # 200 or 302

# Kernel build + tests
cd ~/src/trellis
cargo test --manifest-path kernel/Cargo.toml --lib
python3 -m pytest tests/ -q

# Provider preflight (validates configured providers before a real run — see §4)
python3 -m trellis.provider_check --config <repo_path>/trellis.config.json
```

All green = ready to drive a run.
