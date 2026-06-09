"""Configuration loading and policy management.

Config is loaded from a JSON file and parsed into typed dataclasses.
Policy is a separate hot-reloadable JSON file for runtime tuning.
Both are re-checked each cycle via mtime comparison.
"""

from __future__ import annotations

import hashlib
import json
import logging
import os
import re
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple

from trellis.adapters import ProviderConfig
from trellis.project_paths import (
    PROJECT_CONFIG_FILENAME,
    project_chats_dir,
    project_policy_path,
)
from trellis.runtime.kernel_cli import KernelCliError, run_kernel_cli


# ---------------------------------------------------------------------------
# Dataclasses
# ---------------------------------------------------------------------------

@dataclass
class TmuxConfig:
    session_name: str
    dashboard_window_name: str
    kill_windows_after_capture: bool
    burst_group: Optional[str] = None


@dataclass
class SandboxConfig:
    enabled: bool = True
    backend: str = "bwrap"


@dataclass
class PhaseOverride:
    """Per-phase overrides for actor bindings."""
    worker: Optional[ProviderConfig] = None
    easy_worker: Optional[ProviderConfig] = None
    hard_worker: Optional[ProviderConfig] = None
    blockered_worker: Optional[ProviderConfig] = None
    easy_close_worker: Optional[ProviderConfig] = None
    reviewer: Optional[ProviderConfig] = None


@dataclass
class WorkflowConfig:
    start_phase: str
    paper_tex_path: Optional[Path]
    approved_axioms_path: Path
    allowed_import_prefixes: List[str]
    forbidden_keyword_allowlist: List[str]
    human_input_path: Path
    input_request_path: Path
    main_result_targets: List[Dict[str, Any]] = field(default_factory=list)
    main_result_labels: List[str] = field(default_factory=list)
    phase_overrides: Dict[str, PhaseOverride] = field(default_factory=dict)


@dataclass
class ChatConfig:
    root_dir: Path
    repo_name: str
    project_name: str
    public_base_url: str


@dataclass
class GitConfig:
    remote_url: Optional[str]
    remote_name: str
    branch: str
    author_name: str
    author_email: str


@dataclass
class CorrespondenceAgentConfig:
    """Config for one correspondence/soundness verification agent."""
    provider: str = "claude"
    model: str = "claude-opus-4-6"
    effort: Optional[str] = None  # codex: xhigh, claude: max, etc.
    extra_args: List[str] = field(default_factory=list)
    fallback_models: List[str] = field(default_factory=list)
    label: str = ""  # human-readable label for disagreement reporting


@dataclass
class VerificationConfig:
    """Config for the NL verification model (strongest available, with thinking)."""
    provider: str = "claude"
    model: str = "claude-opus-4-6"
    extra_args: List[str] = field(default_factory=list)
    thinking_budget: str = "high"
    max_context_tokens: int = 50000
    correspondence_agents: List[CorrespondenceAgentConfig] = field(default_factory=list)
    soundness_agents: List[CorrespondenceAgentConfig] = field(default_factory=list)


@dataclass
class Config:
    repo_path: Path
    goal_file: Path
    state_dir: Path
    worker: ProviderConfig
    reviewer: ProviderConfig
    verification: VerificationConfig
    tmux: TmuxConfig
    sandbox: SandboxConfig
    workflow: WorkflowConfig
    chat: ChatConfig
    git: GitConfig
    max_cycles: int
    sleep_seconds: float
    startup_timeout_seconds: float
    burst_timeout_seconds: float
    easy_worker: Optional[ProviderConfig] = None
    hard_worker: Optional[ProviderConfig] = None
    blockered_worker: Optional[ProviderConfig] = None
    easy_close_worker: Optional[ProviderConfig] = None
    policy_path: Optional[Path] = None
    source_path: Optional[Path] = None


# ---------------------------------------------------------------------------
# Policy dataclasses
# ---------------------------------------------------------------------------

@dataclass(frozen=True)
class TimingPolicy:
    sleep_seconds: float = 1.0
    agent_retry_delays_seconds: Tuple[float, ...] = (3600.0, 7200.0, 10800.0)
    budget_error_max_retries: int = 20
    subprocess_timeout_seconds: float = 120.0
    burst_timeout_seconds: float = 14400.0
    stall_threshold_seconds: float = 900.0
    max_stall_recoveries_per_burst: int = 3


@dataclass(frozen=True)
class PromptNotesPolicy:
    initial_theorem_dag_size_min: int = 15
    initial_theorem_dag_size_max: int = 50


@dataclass(frozen=True)
class VerificationPolicy:
    correspondence_agent_selectors: Tuple[str, ...] = ()
    soundness_agent_selectors: Tuple[str, ...] = ()


@dataclass(frozen=True)
class Policy:
    timing: TimingPolicy = field(default_factory=TimingPolicy)
    prompt_notes: PromptNotesPolicy = field(default_factory=PromptNotesPolicy)
    verification: VerificationPolicy = field(default_factory=VerificationPolicy)


# ---------------------------------------------------------------------------
# Validation helpers
# ---------------------------------------------------------------------------

class ConfigError(RuntimeError):
    """Raised for configuration errors."""
    pass


PHASES: Tuple[str, ...] = (
    "paper_check",
    "planning",
    "theorem_stating",
    "proof_formalization",
    "proof_complete_style_cleanup",
)

FORBIDDEN_KEYWORDS_DEFAULT: Tuple[str, ...] = (
    "sorry",
    "axiom",
    "constant",
    "unsafe",
    "opaque",
    "partial",
    "native_decide",
    "implementedBy",
    "implemented_by",
    "extern",
    "elab",
    "macro",
    "syntax",
    "run_cmd",
    "#eval",
)


def _require(raw: Dict[str, Any], key: str, label: str) -> Any:
    if key not in raw:
        raise ConfigError(f"{label} missing required key {key!r}")
    return raw[key]


def _coerce_int(value: Any, label: str, *, minimum: Optional[int] = None) -> int:
    try:
        parsed = int(value)
    except (TypeError, ValueError) as exc:
        raise ConfigError(f"{label} must be an integer, got {value!r}") from exc
    if minimum is not None and parsed < minimum:
        raise ConfigError(f"{label} must be >= {minimum}, got {parsed}")
    return parsed


def _coerce_float(value: Any, label: str, *, minimum: Optional[float] = None, strictly_positive: bool = False) -> float:
    try:
        parsed = float(value)
    except (TypeError, ValueError) as exc:
        raise ConfigError(f"{label} must be numeric, got {value!r}") from exc
    if strictly_positive and parsed <= 0:
        raise ConfigError(f"{label} must be positive, got {parsed}")
    if minimum is not None and parsed < minimum:
        raise ConfigError(f"{label} must be >= {minimum}, got {parsed}")
    return parsed


def _sanitize_name(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9_-]+", "-", value.strip()).strip("-") or "unnamed"


def _sanitize_tmux_name(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9_]+", "_", value.strip()).strip("_") or "session"


def _normalize_main_result_target(raw: Any) -> Dict[str, Any]:
    if isinstance(raw, str):
        label = raw.strip()
        return {"tex_label": label} if label else {}
    if not isinstance(raw, dict):
        return {}

    normalized: Dict[str, Any] = {}
    label = str(raw.get("tex_label", "") or "").strip()
    if label:
        normalized["tex_label"] = label

    try:
        if raw.get("start_line") is not None and raw.get("end_line") is not None:
            normalized["start_line"] = int(raw["start_line"])
            normalized["end_line"] = int(raw["end_line"])
    except (TypeError, ValueError):
        return {"tex_label": label} if label else {}
    return normalized


def format_main_result_target(raw: Any) -> str:
    target = _normalize_main_result_target(raw)
    if not target:
        return "(invalid target)"
    label = str(target.get("tex_label", "") or "").strip()
    if "start_line" in target and "end_line" in target:
        start_line = int(target["start_line"])
        end_line = int(target["end_line"])
        line_text = f"line {start_line}" if start_line == end_line else f"lines {start_line}-{end_line}"
        return f"{label} ({line_text})" if label else line_text
    return label or "(invalid target)"


def resolve_main_result_targets_via_kernel(
    *,
    paper_path: Optional[Path],
    raw_targets: Any = None,
    raw_labels: Any = None,
) -> Dict[str, Any]:
    payload: Dict[str, Any] = {
        "action": "resolve_main_result_targets",
        "paper_path": str(paper_path.resolve()) if paper_path else None,
        "raw_targets": raw_targets,
        "raw_labels": raw_labels,
    }
    try:
        response = run_kernel_cli(payload)
    except KernelCliError as exc:
        raise ConfigError(str(exc)) from exc
    if response.get("status") != "resolve_main_result_targets_ok":
        raise ConfigError(
            "unexpected kernel resolve_main_result_targets response status: "
            f"{response.get('status')!r}"
        )
    output = response.get("output")
    if not isinstance(output, dict):
        raise ConfigError("kernel resolve_main_result_targets response is missing output")
    targets = output.get("targets")
    available_labels = output.get("available_labels")
    preview = output.get("preview")
    if not isinstance(targets, list):
        raise ConfigError("kernel resolve_main_result_targets response is missing targets")
    if not isinstance(available_labels, list):
        raise ConfigError("kernel resolve_main_result_targets response is missing available_labels")
    if not isinstance(preview, list):
        raise ConfigError("kernel resolve_main_result_targets response is missing preview")
    return {
        "targets": targets,
        "available_labels": available_labels,
        "preview": preview,
    }


# ---------------------------------------------------------------------------
# Config loading
# ---------------------------------------------------------------------------

def _parse_provider_config(raw: Any, label: str) -> ProviderConfig:
    if not isinstance(raw, dict):
        raise ConfigError(f"{label} must be a dict")
    provider = str(raw.get("provider", "")).strip().lower()
    if provider not in ("claude", "codex", "gemini"):
        raise ConfigError(
            f"{label}.provider must be claude, codex, or gemini, got {provider!r}"
        )
    return ProviderConfig(
        provider=provider,
        model=raw.get("model") or None,
        effort=raw.get("effort") or None,
        extra_args=list(raw.get("extra_args", [])),
        fallback_models=list(raw.get("fallback_models", [])),
    )


def _provider_config_with_model_override(
    base: ProviderConfig, model_override: Any
) -> ProviderConfig:
    model_text = str(model_override or "").strip() or None
    return ProviderConfig(
        provider=base.provider,
        model=model_text,
        effort=base.effort,
        extra_args=list(base.extra_args),
        fallback_models=list(base.fallback_models),
    )


def _parse_verification_config(raw: Any) -> VerificationConfig:
    if not isinstance(raw, dict):
        return VerificationConfig()
    corr_agents_raw = raw.get("correspondence_agents", [])
    corr_agents: List[CorrespondenceAgentConfig] = []
    if isinstance(corr_agents_raw, list):
        for i, agent_raw in enumerate(corr_agents_raw):
            if isinstance(agent_raw, dict):
                provider = str(agent_raw.get("provider", "claude")).strip().lower()
                if provider not in ("claude", "codex", "gemini"):
                    continue
                raw_model = agent_raw.get("model")
                model = str(raw_model).strip() if raw_model is not None else None
                model = model or None  # empty string -> None
                corr_agents.append(CorrespondenceAgentConfig(
                    provider=provider,
                    model=model,
                    effort=agent_raw.get("effort") or None,
                    extra_args=list(agent_raw.get("extra_args", [])),
                    fallback_models=list(agent_raw.get("fallback_models", [])),
                    label=str(agent_raw.get("label", f"{provider}/{model or 'auto'}")),
                ))
    # Soundness agents (same format as correspondence)
    sound_agents_raw = raw.get("soundness_agents", [])
    sound_agents: List[CorrespondenceAgentConfig] = []
    if isinstance(sound_agents_raw, list):
        for i, agent_raw in enumerate(sound_agents_raw):
            if isinstance(agent_raw, dict):
                provider = str(agent_raw.get("provider", "claude")).strip().lower()
                if provider not in ("claude", "codex", "gemini"):
                    continue
                raw_model = agent_raw.get("model")
                model = str(raw_model).strip() if raw_model is not None else None
                model = model or None
                sound_agents.append(CorrespondenceAgentConfig(
                    provider=provider,
                    model=model,
                    effort=agent_raw.get("effort") or None,
                    extra_args=list(agent_raw.get("extra_args", [])),
                    fallback_models=list(agent_raw.get("fallback_models", [])),
                    label=str(agent_raw.get("label", f"{provider}/{model or 'auto'}")),
                ))

    return VerificationConfig(
        provider=str(raw.get("provider", "claude")).strip().lower(),
        model=str(raw.get("model", "claude-opus-4-6")).strip(),
        extra_args=list(raw.get("extra_args", [])),
        thinking_budget=str(raw.get("thinking_budget", "high")).strip(),
        max_context_tokens=_coerce_int(raw.get("max_context_tokens", 50000), "verification.max_context_tokens", minimum=1000),
        correspondence_agents=corr_agents,
        soundness_agents=sound_agents,
    )


def load_config(path: Path) -> Config:
    """Load and validate a config JSON file."""
    path = path.resolve()
    if not path.exists():
        raise ConfigError(f"Config file not found: {path}")
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise ConfigError(f"Config file is not valid JSON: {path}: {exc}") from exc
    if not isinstance(raw, dict):
        raise ConfigError(f"Config file must contain a JSON object: {path}")

    # repo_path
    repo_path_raw = str(_require(raw, "repo_path", "config"))
    repo_path = (
        (path.parent / repo_path_raw).resolve()
        if not Path(repo_path_raw).is_absolute()
        else Path(repo_path_raw).resolve()
    )
    if not repo_path.is_dir():
        raise ConfigError(f"repo_path does not exist or is not a directory: {repo_path}")

    # goal_file
    goal_file_raw = str(raw.get("goal_file", "GOAL.md"))
    goal_file = (repo_path / goal_file_raw).resolve() if not Path(goal_file_raw).is_absolute() else Path(goal_file_raw).resolve()

    # state_dir
    state_dir_raw = str(raw.get("state_dir", ".trellis"))
    state_dir = (repo_path / state_dir_raw).resolve() if not Path(state_dir_raw).is_absolute() else Path(state_dir_raw).resolve()

    # providers
    worker = _parse_provider_config(_require(raw, "worker", "config"), "config.worker")
    reviewer = _parse_provider_config(_require(raw, "reviewer", "config"), "config.reviewer")
    verification = _parse_verification_config(raw.get("verification", {}))
    easy_worker = _parse_provider_config(raw["easy_worker"], "config.easy_worker") if "easy_worker" in raw else None
    hard_worker = _parse_provider_config(raw["hard_worker"], "config.hard_worker") if "hard_worker" in raw else None
    blockered_worker = (
        _parse_provider_config(raw["blockered_worker"], "config.blockered_worker")
        if "blockered_worker" in raw
        else None
    )
    easy_close_worker = (
        _parse_provider_config(raw["easy_close_worker"], "config.easy_close_worker")
        if "easy_close_worker" in raw
        else None
    )

    # tmux
    tmux_raw = _require(raw, "tmux", "config")
    if not isinstance(tmux_raw, dict):
        raise ConfigError("config.tmux must be a dict")
    tmux = TmuxConfig(
        session_name=_sanitize_tmux_name(str(tmux_raw.get("session_name", "trellis"))),
        dashboard_window_name=str(tmux_raw.get("dashboard_window_name", "dashboard")),
        kill_windows_after_capture=bool(tmux_raw.get("kill_windows_after_capture", True)),
        burst_group=tmux_raw.get("burst_group") or None,
    )

    sandbox_raw = raw.get("sandbox", {})
    if sandbox_raw is None:
        sandbox_raw = {}
    if not isinstance(sandbox_raw, dict):
        raise ConfigError("config.sandbox must be a dict")
    sandbox_backend = str(sandbox_raw.get("backend", "bwrap")).strip().lower() or "bwrap"
    if sandbox_backend not in {"bwrap"}:
        raise ConfigError(f"config.sandbox.backend must be 'bwrap', got {sandbox_backend!r}")
    sandbox = SandboxConfig(
        enabled=bool(sandbox_raw.get("enabled", True)),
        backend=sandbox_backend,
    )

    # workflow
    wf_raw = raw.get("workflow", {})
    if not isinstance(wf_raw, dict):
        raise ConfigError("config.workflow must be a dict")
    start_phase = str(wf_raw.get("start_phase", "paper_check")).strip().lower()
    if start_phase not in PHASES:
        raise ConfigError(f"config.workflow.start_phase must be one of {list(PHASES)}, got {start_phase!r}")
    paper_tex = wf_raw.get("paper_tex_path")
    if paper_tex:
        paper_tex_path: Optional[Path] = (repo_path / paper_tex).resolve()
    else:
        paper_tex_path = None
    main_result_targets_raw = wf_raw.get("main_result_targets", [])
    main_result_labels_raw = wf_raw.get("main_result_labels", [])
    if main_result_targets_raw not in (None, []) and not isinstance(main_result_targets_raw, list):
        raise ConfigError("config.workflow.main_result_targets must be a list")
    if main_result_labels_raw not in (None, []) and not isinstance(main_result_labels_raw, list):
        raise ConfigError("config.workflow.main_result_labels must be a list of strings")
    main_result_resolution = resolve_main_result_targets_via_kernel(
        paper_path=paper_tex_path,
        raw_targets=main_result_targets_raw,
        raw_labels=main_result_labels_raw,
    )
    main_result_targets = main_result_resolution["targets"]
    main_result_labels: List[str] = []
    seen_main_result_labels = set()
    for target in main_result_targets:
        label = str(target.get("tex_label", "") or "").strip()
        if not label or label in seen_main_result_labels:
            continue
        seen_main_result_labels.add(label)
        main_result_labels.append(label)
    # Phase overrides
    phase_overrides_raw = wf_raw.get("phase_overrides", {})
    phase_overrides: Dict[str, PhaseOverride] = {}
    for phase_name, overrides in phase_overrides_raw.items():
        if not isinstance(overrides, dict):
            continue
        phase_worker = (
            _parse_provider_config(overrides["worker"], f"config.workflow.phase_overrides.{phase_name}.worker")
            if "worker" in overrides and overrides.get("worker") is not None
            else None
        )
        phase_easy_worker = (
            _parse_provider_config(
                overrides["easy_worker"],
                f"config.workflow.phase_overrides.{phase_name}.easy_worker",
            )
            if "easy_worker" in overrides and overrides.get("easy_worker") is not None
            else None
        )
        phase_hard_worker = (
            _parse_provider_config(
                overrides["hard_worker"],
                f"config.workflow.phase_overrides.{phase_name}.hard_worker",
            )
            if "hard_worker" in overrides and overrides.get("hard_worker") is not None
            else None
        )
        phase_blockered_worker = (
            _parse_provider_config(
                overrides["blockered_worker"],
                f"config.workflow.phase_overrides.{phase_name}.blockered_worker",
            )
            if "blockered_worker" in overrides and overrides.get("blockered_worker") is not None
            else None
        )
        phase_easy_close_worker = (
            _parse_provider_config(
                overrides["easy_close_worker"],
                f"config.workflow.phase_overrides.{phase_name}.easy_close_worker",
            )
            if "easy_close_worker" in overrides and overrides.get("easy_close_worker") is not None
            else None
        )
        phase_reviewer = (
            _parse_provider_config(
                overrides["reviewer"],
                f"config.workflow.phase_overrides.{phase_name}.reviewer",
            )
            if "reviewer" in overrides and overrides.get("reviewer") is not None
            else None
        )
        if phase_worker is None and "worker_model" in overrides:
            phase_worker = _provider_config_with_model_override(
                worker, overrides.get("worker_model")
            )
        if phase_reviewer is None and "reviewer_model" in overrides:
            phase_reviewer = _provider_config_with_model_override(
                reviewer, overrides.get("reviewer_model")
            )
        phase_overrides[phase_name] = PhaseOverride(
            worker=phase_worker,
            easy_worker=phase_easy_worker,
            hard_worker=phase_hard_worker,
            blockered_worker=phase_blockered_worker,
            easy_close_worker=phase_easy_close_worker,
            reviewer=phase_reviewer,
        )

    workflow = WorkflowConfig(
        start_phase=start_phase,
        paper_tex_path=paper_tex_path,
        approved_axioms_path=(repo_path / str(wf_raw.get("approved_axioms_path", "APPROVED_AXIOMS.json"))).resolve(),
        allowed_import_prefixes=list(wf_raw.get("allowed_import_prefixes", ["Mathlib"])),
        forbidden_keyword_allowlist=list(wf_raw.get("forbidden_keyword_allowlist", [])),
        human_input_path=(repo_path / str(wf_raw.get("human_input_path", "HUMAN_INPUT.md"))).resolve(),
        input_request_path=(repo_path / str(wf_raw.get("input_request_path", "INPUT_REQUEST.md"))).resolve(),
        main_result_targets=main_result_targets,
        main_result_labels=main_result_labels,
        phase_overrides=phase_overrides,
    )

    # chat
    chat_raw = raw.get("chat", {})
    if not isinstance(chat_raw, dict):
        raise ConfigError("config.chat must be a dict")
    chat_root_raw = str(chat_raw.get("root_dir", project_chats_dir(state_dir)))
    chat_root = (
        Path(chat_root_raw).resolve()
        if Path(chat_root_raw).is_absolute()
        else (repo_path / chat_root_raw).resolve()
    )
    chat = ChatConfig(
        root_dir=chat_root,
        repo_name=_sanitize_name(str(chat_raw.get("repo_name", repo_path.name))),
        project_name=str(chat_raw.get("project_name", "") or chat_raw.get("repo_name", repo_path.name)),
        public_base_url=str(chat_raw.get("public_base_url", "https://example.com/trellis-chats/")),
    )

    # git
    git_raw = raw.get("git", {})
    if not isinstance(git_raw, dict):
        raise ConfigError("config.git must be a dict")
    git = GitConfig(
        remote_url=git_raw.get("remote_url") or None,
        # Separate namespace from typical `origin` usage: when `remote_url`
        # is set the supervisor will create/update this named remote and
        # push to it on every checkpoint. Defaulting to a trellis-specific
        # name avoids clobbering any pre-existing `origin` the user may
        # have pointing at their personal fork.
        remote_name=str(git_raw.get("remote_name", "trellis-archive")),
        branch=str(git_raw.get("branch", "main")),
        author_name=str(git_raw.get("author_name", ".trellis")),
        author_email=str(git_raw.get("author_email", "trellis@localhost")),
    )

    # policy_path
    policy_path_raw = raw.get("policy_path")
    if policy_path_raw:
        policy_path: Optional[Path] = Path(policy_path_raw).resolve() if Path(policy_path_raw).is_absolute() else (path.parent / policy_path_raw).resolve()
    else:
        if path.name == PROJECT_CONFIG_FILENAME:
            policy_path = project_policy_path(repo_path).resolve()
        elif path.name.endswith(".config.json"):
            policy_path = path.with_name(path.name.replace(".config.json", ".policy.json")).resolve()
        else:
            policy_path = path.with_suffix(".policy.json").resolve()

    return Config(
        repo_path=repo_path,
        goal_file=goal_file,
        state_dir=state_dir,
        worker=worker,
        reviewer=reviewer,
        verification=verification,
        tmux=tmux,
        sandbox=sandbox,
        workflow=workflow,
        chat=chat,
        git=git,
        max_cycles=_coerce_int(raw.get("max_cycles", 0), "max_cycles", minimum=0),
        sleep_seconds=_coerce_float(raw.get("sleep_seconds", 1.0), "sleep_seconds", minimum=0.0),
        startup_timeout_seconds=_coerce_float(raw.get("startup_timeout_seconds", 3600.0), "startup_timeout_seconds", strictly_positive=True),
        burst_timeout_seconds=_coerce_float(raw.get("burst_timeout_seconds", 600.0), "burst_timeout_seconds", strictly_positive=True),
        easy_worker=easy_worker,
        hard_worker=hard_worker,
        blockered_worker=blockered_worker,
        easy_close_worker=easy_close_worker,
        policy_path=policy_path,
        source_path=path,
    )


# ---------------------------------------------------------------------------
# Policy loading
# ---------------------------------------------------------------------------

def _parse_policy(raw: Any, defaults: Policy, *, path: Path) -> Policy:
    """Parse a raw dict into a Policy, filling missing fields from defaults."""
    if raw is None:
        raw = {}
    if not isinstance(raw, dict):
        raise ConfigError(f"Policy file must contain a JSON object: {path}")

    def _block(key: str) -> Dict[str, Any]:
        val = raw.get(key, {})
        if not isinstance(val, dict):
            raise ConfigError(f"Policy field {key} must be a dict: {path}")
        return val

    tm = _block("timing")
    pn = _block("prompt_notes")
    vf = _block("verification")

    retry_raw = tm.get("agent_retry_delays_seconds", list(defaults.timing.agent_retry_delays_seconds))
    if not isinstance(retry_raw, list):
        raise ConfigError(f"timing.agent_retry_delays_seconds must be a list: {path}")
    retry_delays = tuple(_coerce_float(d, "timing.agent_retry_delays_seconds[]", strictly_positive=True) for d in retry_raw)

    def _selectors(key: str, default: Tuple[str, ...]) -> Tuple[str, ...]:
        raw_val = vf.get(key, list(default))
        if raw_val in (None, ""):
            return ()
        if not isinstance(raw_val, list):
            raise ConfigError(f"verification.{key} must be a list: {path}")
        parsed = tuple(str(v).strip() for v in raw_val if str(v).strip())
        return parsed

    initial_dag_size_min = _coerce_int(
        pn.get(
            "initial_theorem_dag_size_min",
            defaults.prompt_notes.initial_theorem_dag_size_min,
        ),
        "prompt_notes.initial_theorem_dag_size_min",
        minimum=1,
    )
    initial_dag_size_max = _coerce_int(
        pn.get(
            "initial_theorem_dag_size_max",
            defaults.prompt_notes.initial_theorem_dag_size_max,
        ),
        "prompt_notes.initial_theorem_dag_size_max",
        minimum=1,
    )
    if initial_dag_size_min > initial_dag_size_max:
        raise ConfigError(
            f"prompt_notes.initial_theorem_dag_size_min must be <= prompt_notes.initial_theorem_dag_size_max: {path}"
        )

    return Policy(
        timing=TimingPolicy(
            sleep_seconds=_coerce_float(tm.get("sleep_seconds", defaults.timing.sleep_seconds), "timing.sleep_seconds", minimum=0.0),
            agent_retry_delays_seconds=retry_delays,
            budget_error_max_retries=_coerce_int(tm.get("budget_error_max_retries", defaults.timing.budget_error_max_retries), "timing.budget_error_max_retries", minimum=1),
            subprocess_timeout_seconds=_coerce_float(tm.get("subprocess_timeout_seconds", defaults.timing.subprocess_timeout_seconds), "timing.subprocess_timeout_seconds", strictly_positive=True),
            burst_timeout_seconds=_coerce_float(tm.get("burst_timeout_seconds", defaults.timing.burst_timeout_seconds), "timing.burst_timeout_seconds", strictly_positive=True),
            stall_threshold_seconds=_coerce_float(tm.get("stall_threshold_seconds", defaults.timing.stall_threshold_seconds), "timing.stall_threshold_seconds", strictly_positive=True),
            max_stall_recoveries_per_burst=_coerce_int(tm.get("max_stall_recoveries_per_burst", defaults.timing.max_stall_recoveries_per_burst), "timing.max_stall_recoveries_per_burst", minimum=0),
        ),
        prompt_notes=PromptNotesPolicy(
            initial_theorem_dag_size_min=initial_dag_size_min,
            initial_theorem_dag_size_max=initial_dag_size_max,
        ),
        verification=VerificationPolicy(
            correspondence_agent_selectors=_selectors(
                "correspondence_agent_selectors",
                defaults.verification.correspondence_agent_selectors,
            ),
            soundness_agent_selectors=_selectors(
                "soundness_agent_selectors",
                defaults.verification.soundness_agent_selectors,
            ),
        ),
    )


def policy_to_dict(policy: Policy) -> Dict[str, Any]:
    """Serialize a Policy to a plain dict for JSON persistence."""
    return {
        "timing": {"sleep_seconds": policy.timing.sleep_seconds, "agent_retry_delays_seconds": list(policy.timing.agent_retry_delays_seconds), "budget_error_max_retries": policy.timing.budget_error_max_retries, "subprocess_timeout_seconds": policy.timing.subprocess_timeout_seconds, "burst_timeout_seconds": policy.timing.burst_timeout_seconds, "stall_threshold_seconds": policy.timing.stall_threshold_seconds, "max_stall_recoveries_per_burst": policy.timing.max_stall_recoveries_per_burst},
        "prompt_notes": {
            "initial_theorem_dag_size_min": policy.prompt_notes.initial_theorem_dag_size_min,
            "initial_theorem_dag_size_max": policy.prompt_notes.initial_theorem_dag_size_max,
        },
        "verification": {
            "correspondence_agent_selectors": list(policy.verification.correspondence_agent_selectors),
            "soundness_agent_selectors": list(policy.verification.soundness_agent_selectors),
        },
    }


class PolicyManager:
    """Manages hot-reloading of the policy file."""

    def __init__(self, config: Config):
        self.config = config
        self.path = (config.policy_path or config.state_dir / "policy.json").resolve()
        self.defaults = Policy()
        self._policy: Optional[Policy] = None
        self._mtime_ns: Optional[int] = None
        self._digest: Optional[str] = None

    def current(self) -> Policy:
        return self.reload()

    def reload(self, *, force: bool = False) -> Policy:
        """Reload policy from disk if changed. Create default file if missing."""
        self.path.parent.mkdir(parents=True, exist_ok=True)
        if not self.path.exists():
            self.path.write_text(
                json.dumps(policy_to_dict(self.defaults), indent=2) + "\n",
                encoding="utf-8",
            )

        stat = self.path.stat()
        if not force and self._policy is not None and self._mtime_ns == stat.st_mtime_ns:
            return self._policy

        try:
            raw_text = self.path.read_text(encoding="utf-8")
            raw = json.loads(raw_text)
            policy = _parse_policy(raw, self.defaults, path=self.path)
            self._mtime_ns = stat.st_mtime_ns
            self._digest = hashlib.sha256(raw_text.encode("utf-8")).hexdigest()[:16]
            self._policy = policy
            return policy
        except (ConfigError, json.JSONDecodeError) as exc:
            if self._policy is None:
                raise ConfigError(f"Could not load policy {self.path}: {exc}") from exc
            print(f"WARNING: Could not reload policy {self.path}: {exc}. Keeping last known good.")
            return self._policy


# ---------------------------------------------------------------------------
# Config hot-reload support
# ---------------------------------------------------------------------------

class ConfigManager:
    """Manages hot-reloading of the config file."""

    def __init__(self, config: Config):
        self._config = config
        self._path = config.source_path
        self._mtime_ns: Optional[int] = None
        if self._path:
            try:
                self._mtime_ns = self._path.stat().st_mtime_ns
            except OSError:
                pass

    @property
    def config(self) -> Config:
        return self._config

    def check_reload(self) -> bool:
        """Check if config file changed. Returns True if reloaded."""
        if not self._path or not self._path.exists():
            return False
        try:
            stat = self._path.stat()
        except OSError:
            return False
        if self._mtime_ns == stat.st_mtime_ns:
            return False
        try:
            new_config = load_config(self._path)
            self._config = new_config
            self._mtime_ns = stat.st_mtime_ns
            return True
        except ConfigError as exc:
            print(f"WARNING: Could not reload config {self._path}: {exc}. Keeping last known good.")
            return False
