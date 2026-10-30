"""Launch-only worker profiles and capacity; verifier panel policy is separate."""
import copy
import re


DEFAULTS = {
    "worker_slots": 3,
    "max_in_flight_tasks": None,
    "sol_worker_slots": None,
    "heavy_worker_slots": 1,
    "coarse_goal_slots": 2,
    "lightweight_attempts_per_node": 2,
    "compiler_slots": 6,
    "verifier_slots": 8,
    "profiles": {
        "sol-xhigh": {"provider": "codex", "model": "gpt-6.1-sol", "reasoning_effort": "xhigh",
                      "timeout_seconds": 14400, "max_timeout_seconds": 14400,
                      "allowed_tasks": ["ordinary", "lean_closure"], "lightweight": False},
        "luna-high": {"provider": "codex", "model": "gpt-6-luna", "reasoning_effort": "high",
                      "timeout_seconds": 3600, "max_timeout_seconds": 3600,
                      "allowed_tasks": ["lean_closure"], "lightweight": True},
    },
}


def submission_policy(raw, label='worker_submission_policy'):
    if raw is None:
        return None
    if (not isinstance(raw, dict) or set(raw) != {'version', 'max_submissions', 'max_infrastructure_retries'}
            or any(type(value) is not int for value in raw.values())
            or raw['version'] != 1 or not 1 <= raw['max_submissions'] <= 4294967295
            or not 0 <= raw['max_infrastructure_retries'] <= 4294967295):
        raise ValueError(f'{label} requires version 1, positive max_submissions and nonnegative max_infrastructure_retries')
    return copy.deepcopy(raw)


def parse(raw):
    if raw is None:
        return None
    if not isinstance(raw, dict):
        raise ValueError("parallel_dispatch must be an object")
    if set(raw) - ({"enabled"} | set(DEFAULTS) | {"worker_submission_policy"}):
        raise ValueError("unknown parallel_dispatch fields: " + ", ".join(sorted(set(raw) - ({"enabled"} | set(DEFAULTS) | {"worker_submission_policy"}))))
    if type(raw.get("enabled", False)) is not bool:
        raise ValueError("parallel_dispatch.enabled must be a boolean")
    if not raw.get("enabled", False):
        return None
    config = copy.deepcopy(DEFAULTS)
    config.update(copy.deepcopy({k: v for k, v in raw.items() if k != "enabled"}))
    # The heavy-cap default (`heavy_worker_slots` with explicit per-profile
    # `lightweight` designations) replaces the legacy `sol_worker_slots`
    # model-name cap. The shipped default is one heavy worker in a pool of
    # three when the profiles mix a heavy and a lightweight class (the
    # `sol-xhigh` / `luna-high` pair); a configuration whose worker profiles
    # are all one class (for example luna only) defaults to a single worker
    # and no cap, since a cap would merely shrink or duplicate the pool. A
    # configuration written for the legacy model keeps its own capacity
    # meaning: naming `sol_worker_slots`, or supplying a profile without a
    # `lightweight` designation, declines the default cap.
    raw_profiles = raw.get("profiles") if isinstance(raw.get("profiles"), dict) else {}
    profiles_in = raw_profiles if raw_profiles else DEFAULTS["profiles"]
    legacy = ("sol_worker_slots" in raw
              or any(isinstance(p, dict) and "lightweight" not in p for p in profiles_in.values()))
    classes = {p.get("lightweight") for p in profiles_in.values()
               if isinstance(p, dict) and type(p.get("lightweight")) is bool}
    mixed = not legacy and {True, False} <= classes
    if "worker_slots" not in raw and not legacy and not mixed:
        config["worker_slots"] = 1
    if "heavy_worker_slots" not in raw:
        if legacy or not mixed:
            config["heavy_worker_slots"] = None
        elif type(config["worker_slots"]) is int and config["worker_slots"] >= 1:
            config["heavy_worker_slots"] = min(DEFAULTS["heavy_worker_slots"], config["worker_slots"])
    for name in ("worker_slots", "compiler_slots", "verifier_slots"):
        if type(config[name]) is not int or config[name] < 1:
            raise ValueError(f"parallel_dispatch.{name} must be a positive integer")
    if config['max_in_flight_tasks'] is None:
        config.pop('max_in_flight_tasks')
    elif (type(config['max_in_flight_tasks']) is not int
            or config['max_in_flight_tasks'] < config['worker_slots']):
        raise ValueError('parallel_dispatch.max_in_flight_tasks must be at least worker_slots or null')
    if config["coarse_goal_slots"] is None:
        config.pop("coarse_goal_slots")
    elif type(config["coarse_goal_slots"]) is not int or config["coarse_goal_slots"] < 1:
        raise ValueError("parallel_dispatch.coarse_goal_slots must be a positive integer or null")
    if config["sol_worker_slots"] is None:
        config.pop("sol_worker_slots")  # Native optional capacities omit null.
    elif type(config["sol_worker_slots"]) is not int or config["sol_worker_slots"] < 1:
        raise ValueError("parallel_dispatch.sol_worker_slots must be a positive integer or null")
    if config["heavy_worker_slots"] is None:
        config.pop("heavy_worker_slots")
    elif type(config["heavy_worker_slots"]) is not int or config["heavy_worker_slots"] < 1:
        raise ValueError("parallel_dispatch.heavy_worker_slots must be a positive integer or null")
    if config["lightweight_attempts_per_node"] is None:
        config.pop("lightweight_attempts_per_node")
    elif type(config["lightweight_attempts_per_node"]) is not int or config["lightweight_attempts_per_node"] < 1:
        raise ValueError("parallel_dispatch.lightweight_attempts_per_node must be a positive integer or null")
    policy = submission_policy(config.get('worker_submission_policy'), 'parallel_dispatch.worker_submission_policy')
    if policy is None:
        config.pop('worker_submission_policy', None)
    profiles = config["profiles"]
    if not isinstance(profiles, dict) or not profiles:
        raise ValueError("parallel_dispatch.profiles must contain at least one profile")
    for name, profile in profiles.items():
        prefix = f"parallel_dispatch.profiles.{name}"
        if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,96}", name):
            raise ValueError("worker profile IDs must use letters, digits, underscores or hyphens")
        if not isinstance(profile, dict):
            raise ValueError(f"{prefix} must be an object")
        required = {"provider", "model", "reasoning_effort", "timeout_seconds", "max_timeout_seconds"}
        if set(profile) - (required | {"cost_guidance", "allowed_tasks", "lightweight"}) or required - set(profile):
            raise ValueError(f"{prefix} requires provider, model, reasoning_effort, timeout_seconds and max_timeout_seconds; cost_guidance, allowed_tasks and lightweight are optional")
        if 'lightweight' in profile and type(profile['lightweight']) is not bool:
            raise ValueError(f"{prefix}.lightweight must be a boolean")
        if 'heavy_worker_slots' in config and 'lightweight' not in profile:
            raise ValueError(f"{prefix}.lightweight must explicitly designate this profile as light (true) or heavy (false)")
        allowed = profile.setdefault("allowed_tasks", ["lean_closure"])
        if (not isinstance(allowed, list) or not allowed
                or any(t not in ("ordinary", "lean_closure") for t in allowed)
                or len(set(allowed)) != len(allowed)):
            raise ValueError(f"{prefix}.allowed_tasks must be a nonempty list of ordinary and/or lean_closure")
        profile['allowed_tasks'] = [task for task in ('ordinary', 'lean_closure') if task in allowed]
        # Match the native serde representation for launch/restart comparison.
        if not profile.get('cost_guidance'):
            profile.pop('cost_guidance', None)
        for field in ("provider", "model", "reasoning_effort"):
            if not isinstance(profile[field], str) or not profile[field].strip():
                raise ValueError(f"{prefix}.{field} must be a nonempty string")
        for field in ("timeout_seconds", "max_timeout_seconds"):
            if type(profile[field]) is not int or profile[field] < 1:
                raise ValueError(f"{prefix}.{field} must be a positive integer")
        if profile["timeout_seconds"] > profile["max_timeout_seconds"]:
            raise ValueError(f"{prefix}.timeout_seconds exceeds its launch limit")
        if "cost_guidance" in profile and not isinstance(profile["cost_guidance"], str):
            raise ValueError(f"{prefix}.cost_guidance must be a string")
    if policy is not None and any(profile['provider'] not in {'codex', 'deepseek', 'kimi'} for profile in profiles.values()):
        raise ValueError('worker submissions currently require Codex, DeepSeek, or Kimi worker profiles')
    return copy.deepcopy(config)


def resume_settings(settings, durable, raw):
    """Keep an older run's absent cap unless explicitly changed by its operator."""
    result = copy.deepcopy(settings)
    # New defaults must not silently resize existing runs. Explicit settings
    # still have to agree with a drained native capacity event.
    for name in ('worker_slots', 'sol_worker_slots', 'compiler_slots'):
        if name not in raw and name in durable:
            result[name] = durable[name]
    if 'sol_worker_slots' not in durable and 'sol_worker_slots' not in raw:
        result.pop('sol_worker_slots', None)
    if 'coarse_goal_slots' not in durable and 'coarse_goal_slots' not in raw:
        result.pop('coarse_goal_slots', None)
    if 'lightweight_attempts_per_node' not in durable and 'lightweight_attempts_per_node' not in raw:
        result.pop('lightweight_attempts_per_node', None)
    for name, profile in result['profiles'].items():
        if ('lightweight' not in durable.get('profiles', {}).get(name, {})
                and 'lightweight' not in raw.get('profiles', {}).get(name, {})):
            profile.pop('lightweight', None)
    if 'heavy_worker_slots' not in durable and 'heavy_worker_slots' not in raw:
        # A run launched without a heavy cap keeps running without one; the
        # default cap applies to new runs, never to a resume.
        result.pop('heavy_worker_slots', None)
    if 'heavy_worker_slots' in durable:
        # Live controls are recorded in the native event journal. A restart
        # must not restore stale launch-time values or modify agent contracts.
        result['worker_slots'] = durable['worker_slots']
        result['heavy_worker_slots'] = durable['heavy_worker_slots']
    if 'max_in_flight_tasks' in durable and 'max_in_flight_tasks' not in raw:
        result['max_in_flight_tasks'] = durable['max_in_flight_tasks']
    return result
