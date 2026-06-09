from __future__ import annotations

import json
import os
import re
import stat
import subprocess
import textwrap
from pathlib import Path
from unittest.mock import patch

import pytest

from trellis.config import ConfigError, PolicyManager, load_config
from trellis.runtime.kernel_cli import KernelCliError


REPO_ROOT = Path(__file__).resolve().parents[1]


def _write_config(repo: Path, workflow: dict[str, object]) -> Path:
    repo.mkdir(parents=True, exist_ok=True)
    (repo / "paper.tex").write_text("% test paper\n", encoding="utf-8")
    config_path = repo / "trellis.config.json"
    config_path.write_text(
        json.dumps(
            {
                "repo_path": ".",
                "worker": {"provider": "codex"},
                "reviewer": {"provider": "codex"},
                "tmux": {"burst_user": "sandbox-user"},
                "workflow": workflow,
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    return config_path


def _write_fake_kernel(tmp_path: Path) -> tuple[Path, Path]:
    capture_path = tmp_path / "kernel-payload.json"
    kernel_path = tmp_path / "fake_kernel.py"
    kernel_path.write_text(
        """#!/usr/bin/env python3
import json
import os
import sys

payload = json.load(sys.stdin)
capture = os.environ.get("TEST_KERNEL_CAPTURE")
if capture:
    with open(capture, "w", encoding="utf-8") as handle:
        json.dump(payload, handle)

if payload.get("action") != "resolve_main_result_targets":
    print(json.dumps({"message": f"unexpected action: {payload.get('action')!r}"}))
    sys.exit(1)

raw_labels = payload.get("raw_labels")
if raw_labels == ["missing"]:
    print(
        json.dumps(
            {
                "message": (
                    "Configured main_result_labels are not present as labeled paper statements: "
                    "missing"
                )
            }
        )
    )
    sys.exit(1)

target = {"tex_label": "thm:main", "start_line": 3, "end_line": 5}
print(
    json.dumps(
        {
            "status": "resolve_main_result_targets_ok",
            "output": {
                "targets": [target],
                "available_labels": ["thm:extra", "thm:main"],
                "preview": [
                    {
                        "target": target,
                        "env": "theorem",
                        "text": "Main statement.",
                        "start_line": 3,
                        "end_line": 5,
                    }
                ],
            },
        }
    )
)
""",
        encoding="utf-8",
    )
    kernel_path.chmod(kernel_path.stat().st_mode | stat.S_IXUSR)
    return kernel_path, capture_path


def _write_setup_runtime_kernel(tmp_path: Path) -> Path:
    kernel_path = tmp_path / "fake_setup_kernel.py"
    kernel_path.write_text(
        textwrap.dedent(
            """\
            #!/usr/bin/env python3
            import json
            import sys

            payload = json.load(sys.stdin)
            action = payload.get("action")

            if action == "resolve_main_result_targets":
                target = {"tex_label": "thm:main", "start_line": 1, "end_line": 1}
                print(
                    json.dumps(
                        {
                            "status": "resolve_main_result_targets_ok",
                            "output": {
                                "targets": [target],
                                "available_labels": ["thm:main"],
                                "preview": [
                                    {
                                        "target": target,
                                        "env": "theorem",
                                        "text": "Main statement.",
                                        "start_line": 1,
                                        "end_line": 1,
                                    }
                                ],
                            },
                        }
                    )
                )
                raise SystemExit(0)

            if action == "sync_tablet_support":
                print(json.dumps({"status": "sync_tablet_support_ok", "output": {}}))
                raise SystemExit(0)

            if action == "check_tablet":
                print(
                    json.dumps(
                        {
                            "status": "check_tablet_ok",
                            "output": {"ok": True, "errors": [], "warnings": []},
                        }
                    )
                )
                raise SystemExit(0)

            if action == "check_node":
                print(
                    json.dumps(
                        {
                            "status": "check_node_ok",
                            "output": {"ok": True, "errors": [], "warnings": []},
                        }
                    )
                )
                raise SystemExit(0)

            print(json.dumps({"message": f"unexpected action: {action!r}"}))
            raise SystemExit(1)
            """
        ),
        encoding="utf-8",
    )
    kernel_path.chmod(kernel_path.stat().st_mode | stat.S_IXUSR)
    return kernel_path


def _write_fake_setup_toolchain(tmp_path: Path) -> Path:
    bin_dir = tmp_path / "fake-bin"
    bin_dir.mkdir()

    (bin_dir / "sudo").write_text(
        textwrap.dedent(
            """\
            #!/usr/bin/env python3
            import os
            import subprocess
            import sys
            from pathlib import Path

            args = sys.argv[1:]
            user = ""
            group = ""
            command = []
            i = 0
            while i < len(args):
                arg = args[i]
                if arg == "-n":
                    i += 1
                    continue
                if arg == "-u":
                    user = args[i + 1]
                    i += 2
                    continue
                if arg == "-g":
                    group = args[i + 1]
                    i += 2
                    continue
                if arg == "env":
                    i += 1
                    while i < len(args) and "=" in args[i] and not args[i].startswith("-"):
                        key, value = args[i].split("=", 1)
                        os.environ[key] = value
                        i += 1
                    command = args[i:]
                    break
                command = args[i:]
                break

            env = os.environ.copy()
            if user:
                env["FAKE_EFFECTIVE_USER"] = user
            if group:
                env["FAKE_EFFECTIVE_GROUP"] = group
            fakebin = env.get("TEST_FAKEBIN", "").strip()
            if fakebin:
                env["PATH"] = f"{fakebin}:{env.get('PATH', '')}" if env.get("PATH") else fakebin
            log_path = env.get("TEST_SUDO_LOG", "").strip()
            if log_path:
                Path(log_path).parent.mkdir(parents=True, exist_ok=True)
                with open(log_path, "a", encoding="utf-8") as handle:
                    handle.write(
                        f"user={user or '-'} group={group or '-'} cmd={' '.join(command)}\\n"
                    )

            result = subprocess.run(command, env=env, check=False)
            raise SystemExit(result.returncode)
            """
        ),
        encoding="utf-8",
    )

    (bin_dir / "bwrap").write_text(
        textwrap.dedent(
            """\
            #!/usr/bin/env python3
            import os
            import subprocess
            import sys

            args = sys.argv[1:]
            i = 0
            while i < len(args):
                arg = args[i]
                if arg in {"--die-with-parent"}:
                    i += 1
                    continue
                # Phase 1 (SANDBOX_BWRAP_ONLY_MIGRATION_PLAN_2026-06-03.md
                # §3) added bwrap hardening flags: namespace isolation +
                # explicit cap-drop. The fake bwrap mimics the parser by
                # accepting + ignoring them so the probe still execs the
                # inner command on this host's PID namespace.
                if arg in {"--unshare-pid", "--unshare-ipc", "--unshare-uts"}:
                    i += 1
                    continue
                if arg in {"--proc", "--tmpfs", "--dir", "--chdir", "--cap-drop"}:
                    i += 2
                    continue
                if arg in {"--dev-bind", "--ro-bind", "--bind", "--setenv"}:
                    if arg == "--setenv":
                        os.environ[args[i + 1]] = args[i + 2]
                    i += 3
                    continue
                break
            result = subprocess.run(args[i:], env=os.environ.copy(), check=False)
            raise SystemExit(result.returncode)
            """
        ),
        encoding="utf-8",
    )

    (bin_dir / "lake").write_text(
        textwrap.dedent(
            """\
            #!/usr/bin/env python3
            import os
            import sys
            from pathlib import Path

            log_path = os.environ.get("TEST_LAKE_LOG", "").strip()
            if log_path:
                with open(log_path, "a", encoding="utf-8") as handle:
                    handle.write(
                        f"user={os.environ.get('FAKE_EFFECTIVE_USER', '-')}"
                        f" cwd={Path.cwd()} args={' '.join(sys.argv[1:])}\\n"
                    )

            expected_user = os.environ.get("TEST_EXPECT_BURST_USER", "").strip()
            effective_user = os.environ.get("FAKE_EFFECTIVE_USER", "").strip()
            if expected_user and effective_user != expected_user:
                print(
                    "error: permission denied writing shared .lake state; expected burst-user prewarm",
                    file=sys.stderr,
                )
                raise SystemExit(13)

            repo = Path.cwd()
            (repo / ".lake" / "packages" / "mathlib").mkdir(parents=True, exist_ok=True)
            (repo / ".lake" / "build").mkdir(parents=True, exist_ok=True)
            raise SystemExit(0)
            """
        ),
        encoding="utf-8",
    )

    for name, body in {
        "codex": "#!/bin/bash\nexit 0\n",
        "claude": "#!/bin/bash\nexit 0\n",
        "gemini": "#!/bin/bash\nexit 0\n",
        "lean": "#!/bin/bash\nexit 0\n",
        "touch": "#!/bin/bash\nif [ \"$1\" = \"__trellis_sandbox_repo_root_probe\" ]; then exit 1; fi\nexec /usr/bin/touch \"$@\"\n",
    }.items():
        (bin_dir / name).write_text(body, encoding="utf-8")

    for path in bin_dir.iterdir():
        path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return bin_dir


def test_load_config_uses_kernel_for_main_result_resolution(tmp_path: Path) -> None:
    config_path = _write_config(
        tmp_path / "repo",
        {
            "paper_tex_path": "paper.tex",
            "main_result_targets": [{"tex_label": "thm:explicit"}],
            "main_result_labels": ["thm:label-selector"],
        },
    )

    with patch(
        "trellis.config.run_kernel_cli",
        return_value={
            "status": "resolve_main_result_targets_ok",
            "output": {
                "targets": [{"tex_label": "thm:explicit", "start_line": 11, "end_line": 15}],
                "available_labels": ["thm:explicit", "thm:label-selector"],
                "preview": [],
            },
        },
    ) as mock_kernel:
        config = load_config(config_path)

    payload = mock_kernel.call_args.args[0]
    assert payload["action"] == "resolve_main_result_targets"
    assert payload["paper_path"] == str((config.repo_path / "paper.tex").resolve())
    assert payload["raw_targets"] == [{"tex_label": "thm:explicit"}]
    assert payload["raw_labels"] == ["thm:label-selector"]
    assert config.workflow.main_result_targets == [
        {"tex_label": "thm:explicit", "start_line": 11, "end_line": 15}
    ]
    assert config.workflow.main_result_labels == ["thm:explicit"]


def test_load_config_surfaces_kernel_missing_label_error(tmp_path: Path) -> None:
    config_path = _write_config(
        tmp_path / "repo",
        {
            "paper_tex_path": "paper.tex",
            "main_result_labels": ["missing"],
        },
    )

    with patch(
        "trellis.config.run_kernel_cli",
        side_effect=KernelCliError(
            "Configured main_result_labels are not present as labeled paper statements: missing"
        ),
    ):
        with pytest.raises(
            ConfigError,
            match=(
                "Configured main_result_labels are not present as labeled paper statements: missing"
            ),
        ):
            load_config(config_path)


def test_load_config_parses_phase_specific_worker_overrides(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    repo.mkdir(parents=True, exist_ok=True)
    (repo / "paper.tex").write_text("% test paper\n", encoding="utf-8")
    config_path = repo / "trellis.config.json"
    config_path.write_text(
        json.dumps(
            {
                "repo_path": ".",
                "worker": {"provider": "codex", "model": "global-worker"},
                "easy_worker": {"provider": "gemini", "model": "global-easy"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"},
                "tmux": {"burst_user": "sandbox-user"},
                "workflow": {
                    "start_phase": "theorem_stating",
                    "paper_tex_path": "paper.tex",
                    "phase_overrides": {
                        "theorem_stating": {
                            "worker": {"provider": "claude", "model": "theorem-worker"}
                        },
                        "proof_formalization": {
                            "easy_worker": {"provider": "gemini", "model": "proof-easy"},
                            "hard_worker": {"provider": "codex", "model": "proof-hard"},
                            "reviewer": {"provider": "claude", "model": "proof-reviewer"},
                        },
                    },
                },
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )

    with patch(
        "trellis.config.run_kernel_cli",
        return_value={
            "status": "resolve_main_result_targets_ok",
            "output": {"targets": [], "available_labels": [], "preview": []},
        },
    ):
        config = load_config(config_path)

    theorem_override = config.workflow.phase_overrides["theorem_stating"]
    assert theorem_override.worker is not None
    assert theorem_override.worker.provider == "claude"
    assert theorem_override.worker.model == "theorem-worker"

    proof_override = config.workflow.phase_overrides["proof_formalization"]
    assert proof_override.easy_worker is not None
    assert proof_override.easy_worker.provider == "gemini"
    assert proof_override.easy_worker.model == "proof-easy"
    assert proof_override.hard_worker is not None
    assert proof_override.hard_worker.provider == "codex"
    assert proof_override.hard_worker.model == "proof-hard"
    assert proof_override.reviewer is not None
    assert proof_override.reviewer.provider == "claude"
    assert proof_override.reviewer.model == "proof-reviewer"


def test_load_config_parses_blockered_worker_root_and_phase_override(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    repo.mkdir(parents=True, exist_ok=True)
    (repo / "paper.tex").write_text("% test paper\n", encoding="utf-8")
    config_path = repo / "trellis.config.json"
    config_path.write_text(
        json.dumps(
            {
                "repo_path": ".",
                "worker": {"provider": "gemini", "model": "global-worker"},
                "easy_worker": {"provider": "gemini", "model": "global-easy"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "blockered_worker": {
                    "provider": "codex",
                    "model": "global-blockered",
                    "effort": "xhigh",
                },
                "reviewer": {"provider": "claude", "model": "global-reviewer"},
                "tmux": {"burst_user": "sandbox-user"},
                "workflow": {
                    "start_phase": "proof_formalization",
                    "paper_tex_path": "paper.tex",
                    "phase_overrides": {
                        "proof_formalization": {
                            "blockered_worker": {
                                "provider": "codex",
                                "model": "phase-blockered",
                                "effort": "xhigh",
                            }
                        },
                    },
                },
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )

    with patch(
        "trellis.config.run_kernel_cli",
        return_value={
            "status": "resolve_main_result_targets_ok",
            "output": {"targets": [], "available_labels": [], "preview": []},
        },
    ):
        config = load_config(config_path)

    assert config.blockered_worker is not None
    assert config.blockered_worker.provider == "codex"
    assert config.blockered_worker.model == "global-blockered"
    assert config.blockered_worker.effort == "xhigh"

    proof_override = config.workflow.phase_overrides["proof_formalization"]
    assert proof_override.blockered_worker is not None
    assert proof_override.blockered_worker.model == "phase-blockered"


def test_setup_repo_preview_uses_kernel_default_inference(tmp_path: Path) -> None:
    kernel_path, capture_path = _write_fake_kernel(tmp_path)
    paper_path = tmp_path / "paper.tex"
    paper_path.write_text("\\begin{theorem}Main statement.\\end{theorem}\n", encoding="utf-8")
    config_template = tmp_path / "config-template.json"
    config_template.write_text("{}\n", encoding="utf-8")
    repo_path = tmp_path / "new-project"

    env = dict(os.environ)
    env["CONFIG_TEMPLATE"] = str(config_template)
    env["TRELLIS_TRELLIS_KERNEL_CMD"] = str(kernel_path)
    env["TEST_KERNEL_CAPTURE"] = str(capture_path)

    result = subprocess.run(
        ["bash", "scripts/setup_repo.sh", "--loogle", "off", str(repo_path), str(paper_path)],
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 1
    assert "Resolved main-result targets:" in result.stdout
    assert "1. thm:main (lines 3-5) [theorem]" in result.stdout
    assert "Main statement." in result.stdout
    assert "setup requires target confirmation" in result.stderr

    payload = json.loads(capture_path.read_text(encoding="utf-8"))
    assert payload["action"] == "resolve_main_result_targets"
    assert payload["raw_labels"] is None
    assert payload["raw_targets"] is None
    # `setup_repo.sh` first runs `scripts/normalize_paper_envs.py` to rewrite
    # \newtheorem aliases into canonical env names, writing a normalized copy to
    # a `mktemp` scratch file under `$SETUP_SCRATCH_ROOT` (default
    # `<source_root>/.trellis/setup_repo_tmp`). The kernel resolver therefore
    # sees the normalized temp path, not the original paper path. The mktemp
    # template is `${PAPER_NAME%.tex}.normalized.XXXXXX.tex`.
    resolved_paper_path = Path(payload["paper_path"])
    expected_scratch_root = REPO_ROOT / ".trellis" / "setup_repo_tmp"
    assert resolved_paper_path.parent == expected_scratch_root.resolve()
    assert re.fullmatch(
        r"paper\.normalized\.[A-Za-z0-9]{6}\.tex", resolved_paper_path.name
    ), resolved_paper_path.name


def test_setup_repo_preserves_missing_label_error(tmp_path: Path) -> None:
    kernel_path, capture_path = _write_fake_kernel(tmp_path)
    paper_path = tmp_path / "paper.tex"
    paper_path.write_text("\\begin{theorem}Main statement.\\end{theorem}\n", encoding="utf-8")
    config_template = tmp_path / "config-template.json"
    config_template.write_text("{}\n", encoding="utf-8")
    repo_path = tmp_path / "new-project"

    env = dict(os.environ)
    env["CONFIG_TEMPLATE"] = str(config_template)
    env["TRELLIS_TRELLIS_KERNEL_CMD"] = str(kernel_path)
    env["TEST_KERNEL_CAPTURE"] = str(capture_path)

    result = subprocess.run(
        [
            "bash",
            "scripts/setup_repo.sh",
            "--loogle",
            "off",
            "--main-result-labels",
            "missing",
            str(repo_path),
            str(paper_path),
        ],
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode != 0
    assert (
        "Configured main_result_labels are not present as labeled paper statements: missing"
        in result.stderr
    )

    payload = json.loads(capture_path.read_text(encoding="utf-8"))
    assert payload["action"] == "resolve_main_result_targets"
    assert payload["raw_labels"] == ["missing"]
    assert payload["raw_targets"] is None


def test_setup_repo_reset_prewarms_without_sudo_wrap(tmp_path: Path) -> None:
    """Phase 4 bwrap-only migration: setup_repo.sh no longer wraps the
    prewarm pass (or lake/provider-CLI checks) in `sudo -n -u
    burst_user`. The prewarm now runs as the supervisor user directly. The fake
    `sudo` binary in `_write_fake_setup_toolchain` would still record
    any sudo invocations; this test asserts the sudo log stays empty
    while lake itself is invoked.
    """
    config_template = REPO_ROOT / ".trellis-smoke" / "templates" / "example-run.config.json"
    policy_template = REPO_ROOT / ".trellis-smoke" / "templates" / "example-run.policy.json"
    # On a worktree where the smoke templates are untracked-and-missing
    # (they live in main as untracked files), the test is unrunnable —
    # skip rather than spuriously fail.
    if not config_template.is_file() or not policy_template.is_file():
        import pytest
        pytest.skip(
            "untracked fixture missing: .trellis-smoke/templates/example-run.{config,policy}.json"
        )

    kernel_path = _write_setup_runtime_kernel(tmp_path)
    fake_bin = _write_fake_setup_toolchain(tmp_path)
    paper_path = tmp_path / "paper.tex"
    paper_path.write_text("\\begin{theorem}Main statement.\\end{theorem}\n", encoding="utf-8")
    repo_path = tmp_path / "project"
    static_out = tmp_path / "static"
    setup_scratch = tmp_path / "setup-scratch"
    burst_home = tmp_path / "burst-home"
    burst_home.mkdir()
    elan_home = tmp_path / "elan-home"
    elan_home.mkdir()
    sudo_log = tmp_path / "sudo.log"
    lake_log = tmp_path / "lake.log"

    env = dict(os.environ)
    env["PATH"] = f"{fake_bin}:{env.get('PATH', '')}"
    env["TEST_FAKEBIN"] = str(fake_bin)
    env["TEST_SUDO_LOG"] = str(sudo_log)
    env["TEST_LAKE_LOG"] = str(lake_log)
    # Phase 4: lake runs directly as the supervisor user. The fake lake binary
    # checks TEST_EXPECT_BURST_USER against FAKE_EFFECTIVE_USER; an
    # empty TEST_EXPECT_BURST_USER disables the check, which is the
    # right post-Phase-4 expectation (no sudo wrap, no effective-user
    # change).
    env["TEST_EXPECT_BURST_USER"] = ""
    env["CONFIG_TEMPLATE"] = str(config_template)
    env["POLICY_TEMPLATE"] = str(policy_template)
    env["TRELLIS_TRELLIS_KERNEL_CMD"] = str(kernel_path)
    env["STATIC_OUT"] = str(static_out)
    env["SETUP_SCRATCH_ROOT"] = str(setup_scratch)
    env["BURST_HOME"] = str(burst_home)
    env["BURST_PATH"] = f"{fake_bin}:/usr/bin:/bin"
    env["ELAN_HOME"] = str(elan_home)

    result = subprocess.run(
        [
            "bash",
            "scripts/setup_repo.sh",
            "--loogle",
            "off",
            "--reset",
            "--yes",
            str(repo_path),
            str(paper_path),
            "synthetic-setup-test",
        ],
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 0, result.stderr or result.stdout
    assert "Setup complete." in result.stdout
    assert (repo_path / "trellis.config.json").is_file()
    lake_lines = lake_log.read_text(encoding="utf-8").splitlines()
    assert lake_lines, "lake was never invoked by the prewarm"
    # Phase 4: lake runs without an effective-user change. No lake
    # invocation should carry a sudo effective-user (the fake shims log
    # `user=-` for a direct, non-sudo call).
    for line in lake_lines:
        assert "user=sandbox-user" not in line, (
            f"unexpected sudo wrap routed lake through the burst user: {line!r}"
        )
    # The fake sudo log should be empty (or at most carry no `lake`
    # invocations) — setup_repo.sh no longer wraps in sudo.
    sudo_log_text = (
        sudo_log.read_text(encoding="utf-8") if sudo_log.exists() else ""
    )
    assert "lake update" not in sudo_log_text, (
        f"setup_repo.sh still routes lake through sudo: {sudo_log_text!r}"
    )


def test_policy_parses_initial_theorem_dag_size_guidance_range(tmp_path: Path) -> None:
    config_path = _write_config(tmp_path / "repo", {})
    policy_path = config_path.parent / "trellis.policy.json"
    policy_path.write_text(
        json.dumps(
            {
                "prompt_notes": {
                    "initial_theorem_dag_size_min": 20,
                    "initial_theorem_dag_size_max": 60,
                }
            }
        )
        + "\n",
        encoding="utf-8",
    )

    with patch(
        "trellis.config.run_kernel_cli",
        return_value={
            "status": "resolve_main_result_targets_ok",
            "output": {
                "targets": [],
                "available_labels": [],
                "preview": [],
            },
        },
    ):
        config = load_config(config_path)

    policy = PolicyManager(config).current()
    assert policy.prompt_notes.initial_theorem_dag_size_min == 20
    assert policy.prompt_notes.initial_theorem_dag_size_max == 60
