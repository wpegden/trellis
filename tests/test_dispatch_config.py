import pytest

from trellis.dispatch.config import parse, DEFAULTS


def test_opt_in_profiles_and_independent_capacity():
    assert parse(None) is None
    assert parse({"enabled": False}) is None
    config = parse({"enabled": True})
    assert (config["worker_slots"], config["compiler_slots"], config["verifier_slots"]) == (3, 6, 8)
    assert config["profiles"]["sol-xhigh"]["model"] == "gpt-6.1-sol"
    assert config["profiles"]["luna-high"]["model"] == "gpt-6-luna"
    assert config["profiles"]["sol-xhigh"]["timeout_seconds"] == 14400
    assert config["profiles"]["luna-high"]["max_timeout_seconds"] == 3600
    assert config["profiles"]["luna-high"]["allowed_tasks"] == ["lean_closure"]
    assert config["profiles"]["sol-xhigh"]["allowed_tasks"] == ["ordinary", "lean_closure"]
    config["profiles"]["sol-xhigh"]["timeout_seconds"] = 1
    assert parse({"enabled": True})["profiles"]["sol-xhigh"]["timeout_seconds"] == 14400


def test_launch_can_allow_longer_productive_sol_bursts_and_explicit_cost_guidance():
    profile = parse({"enabled": True})["profiles"]["sol-xhigh"]
    profile.update(timeout_seconds=21600, max_timeout_seconds=28800, cost_guidance="Configured premium reasoning lane.")
    config = parse({"enabled": True, "profiles": {"long-sol": profile}, "worker_slots": 6})
    assert config["profiles"]["long-sol"]["timeout_seconds"] == 21600
    assert "luna-high" not in config["profiles"]  # launch menu replaces defaults


@pytest.mark.parametrize("raw", [
    {"enabled": "yes"}, {"enabled": True, "worker_slots": True},
    {"enabled": True, "compiler_slots": 0}, {"enabled": True, "profiles": {}},
    {"enabled": True, "profiles": {"../unsafe": {}}},
    {"enabled": True, "unrecognized": 1},
])
def test_bad_launch_config_is_clear(raw):
    with pytest.raises(ValueError, match="parallel_dispatch|profile"):
        parse(raw)


def test_custom_profile_permissions_are_conservative_and_explicitly_configurable():
    profile = parse({"enabled": True})["profiles"]["luna-high"]
    profile.pop("allowed_tasks")
    assert parse({"enabled": True, "profiles": {"cheap": profile}})["profiles"]["cheap"]["allowed_tasks"] == ["lean_closure"]
    assert "allowed_tasks" not in profile
    profile["allowed_tasks"] = ["ordinary", "lean_closure"]
    assert "ordinary" in parse({"enabled": True, "profiles": {"custom": profile}})["profiles"]["custom"]["allowed_tasks"]


def test_heavy_cap_is_the_default_and_legacy_sol_configurations_keep_their_model():
    from trellis.dispatch.config import resume_settings
    settings = parse({"enabled": True})
    assert 'sol_worker_slots' not in settings
    assert (settings['worker_slots'], settings['heavy_worker_slots']) == (3, 1)
    assert parse({"enabled": True, "worker_slots": 8})['heavy_worker_slots'] == 1  # the cap does not grow with the pool
    assert parse({"enabled": True, "worker_slots": 1})['heavy_worker_slots'] == 1  # never above the pool
    # Worker profiles of one class (luna only, or sol only) default to a single worker and no cap.
    luna_only = {"luna-high": DEFAULTS['profiles']['luna-high']}
    sol_only = {"sol-xhigh": DEFAULTS['profiles']['sol-xhigh']}
    for single in (luna_only, sol_only):
        one = parse({"enabled": True, "profiles": single})
        assert one['worker_slots'] == 1 and 'heavy_worker_slots' not in one
        assert parse({"enabled": True, "profiles": single, "worker_slots": 4})['worker_slots'] == 4
        assert parse({"enabled": True, "profiles": single, "heavy_worker_slots": 2})['heavy_worker_slots'] == 2
    assert parse({"enabled": True, "profiles": dict(luna_only, **sol_only)})['heavy_worker_slots'] == 1
    # Naming the legacy cap keeps the legacy model: no default heavy cap appears.
    legacy = parse({"enabled": True, "sol_worker_slots": 4})
    assert legacy['sol_worker_slots'] == 4 and 'heavy_worker_slots' not in legacy
    assert 'sol_worker_slots' not in parse({"enabled": True, "sol_worker_slots": None})
    assert 'heavy_worker_slots' not in parse({"enabled": True, "sol_worker_slots": None})
    # Profiles without an explicit lightweight designation also decline the default cap.
    undesignated = {name: {k: v for k, v in profile.items() if k != 'lightweight'}
                    for name, profile in DEFAULTS['profiles'].items()}
    assert 'heavy_worker_slots' not in parse({"enabled": True, "profiles": undesignated})
    # An explicit heavy cap still wins, and an explicit null means no cap.
    assert parse({"enabled": True, "heavy_worker_slots": 2})['heavy_worker_slots'] == 2
    assert 'heavy_worker_slots' not in parse({"enabled": True, "heavy_worker_slots": None})
    # Resume: a run launched without any cap stays uncapped; a recorded legacy cap is kept.
    uncapped = dict(settings); uncapped.pop('heavy_worker_slots')
    assert resume_settings(settings, uncapped, {"enabled": True}) == uncapped
    assert resume_settings(settings, settings, {"enabled": True}) == settings
    assert resume_settings(settings, legacy, {"enabled": True}) == legacy
    assert resume_settings(settings, uncapped, {"enabled": True, "heavy_worker_slots": 2}) != uncapped
    assert settings['heavy_worker_slots'] == 1


@pytest.mark.parametrize('value', [0, -1, True, '2', 1.5])
def test_bad_sol_capacity(value):
    with pytest.raises(ValueError, match='sol_worker_slots'):
        parse({"enabled": True, "sol_worker_slots": value})


def test_coarse_capacity_is_configurable_and_old_runs_require_explicit_migration():
    from trellis.dispatch.config import resume_settings
    settings = parse({'enabled': True})
    assert settings['coarse_goal_slots'] == 2
    assert parse({'enabled': True, 'coarse_goal_slots': 4})['coarse_goal_slots'] == 4
    assert 'coarse_goal_slots' not in parse({'enabled': True, 'coarse_goal_slots': None})
    legacy = dict(settings); legacy.pop('coarse_goal_slots')
    assert resume_settings(settings, legacy, {'enabled': True}) == legacy
    assert resume_settings(settings, legacy, {'enabled': True, 'coarse_goal_slots': 2}) == settings
    for value in [False, 0, -1, '2', 1.5]:
        with pytest.raises(ValueError, match='coarse_goal_slots'):
            parse({'enabled': True, 'coarse_goal_slots': value})


def test_lightweight_designation_and_budget_are_explicit_and_legacy_safe():
    from copy import deepcopy
    from trellis.dispatch.config import resume_settings
    settings = parse({'enabled': True})
    assert settings['lightweight_attempts_per_node'] == 2
    assert settings['profiles']['luna-high']['lightweight'] is True
    assert settings['profiles']['sol-xhigh']['lightweight'] is False
    assert parse({'enabled': True, 'lightweight_attempts_per_node': 3})['lightweight_attempts_per_node'] == 3
    assert 'lightweight_attempts_per_node' not in parse({'enabled': True, 'lightweight_attempts_per_node': None})
    legacy = deepcopy(settings); legacy.pop('lightweight_attempts_per_node')
    for profile in legacy['profiles'].values():
        profile.pop('lightweight')
    assert resume_settings(settings, legacy, {'enabled': True}) == legacy
    explicit = {'enabled': True, 'profiles': settings['profiles'], 'lightweight_attempts_per_node': 2}
    assert resume_settings(settings, legacy, explicit) == settings
    assert 'lightweight' not in parse({'enabled': True, 'profiles': legacy['profiles']})['profiles']['luna-high']
    custom = deepcopy(settings['profiles']['luna-high']); custom['model'] = 'arbitrary-model'; custom['lightweight'] = False
    assert parse({'enabled': True, 'profiles': {'luna-high': custom}})['profiles']['luna-high']['lightweight'] is False


@pytest.mark.parametrize('value', [0, -1, True, '2', 1.5])
def test_bad_lightweight_budget(value):
    with pytest.raises(ValueError, match='lightweight_attempts_per_node'):
        parse({'enabled': True, 'lightweight_attempts_per_node': value})


@pytest.mark.parametrize('value', [None, 0, 'true', [], {}])
def test_bad_lightweight_designation(value):
    profile = parse({'enabled': True})['profiles']['luna-high']; profile['lightweight'] = value
    with pytest.raises(ValueError, match='lightweight must be a boolean'):
        parse({'enabled': True, 'profiles': {'custom': profile}})


def test_worker_submission_policy_is_explicit_configurable_and_legacy_safe():
    policy = dict(version=1, max_submissions=3, max_infrastructure_retries=2)
    assert 'worker_submission_policy' not in parse({'enabled': True})
    assert 'worker_submission_policy' not in parse({'enabled': True, 'worker_submission_policy': None})
    assert parse({'enabled': True, 'worker_submission_policy': policy})['worker_submission_policy'] == policy
    for invalid in [{}, dict(policy, max_submissions=0), dict(policy, version=True),
                    dict(policy, max_infrastructure_retries=-1), dict(policy, unknown=1)]:
        with pytest.raises(ValueError, match='worker_submission_policy'):
            parse({'enabled': True, 'worker_submission_policy': invalid})
    profiles = parse({'enabled': True})['profiles']
    profiles['sol-xhigh']['provider'] = 'unsupported'
    with pytest.raises(ValueError, match='require Codex'):
        parse({'enabled': True, 'profiles': profiles, 'worker_submission_policy': policy})


def test_explicit_historical_capacities_are_not_raised_by_new_defaults():
    from trellis.dispatch.config import resume_settings
    raw = dict(enabled=True, worker_slots=4, sol_worker_slots=2, compiler_slots=2)
    historical = parse(raw)
    assert (historical['worker_slots'], historical['sol_worker_slots'], historical['compiler_slots']) == (4, 2, 2)
    assert resume_settings(parse(raw), historical, raw) == historical


def test_implicit_historical_capacities_resume_without_silent_resize():
    from trellis.dispatch.config import resume_settings
    historical = parse(dict(enabled=True, worker_slots=4, sol_worker_slots=2, compiler_slots=2))
    raw = dict(enabled=True)
    assert resume_settings(parse(raw), historical, raw) == historical
    raw['worker_slots'] = 8
    assert resume_settings(parse(raw), historical, raw) != historical


def test_live_heavy_capacity_is_explicit_and_survives_stale_launch_defaults():
    from copy import deepcopy
    from trellis.dispatch.config import resume_settings
    raw = {'enabled': True, 'worker_slots': 8, 'sol_worker_slots': 6}
    launch = parse(raw)
    durable = deepcopy(launch)
    durable.update(worker_slots=2, heavy_worker_slots=1)
    assert resume_settings(launch, durable, raw) == durable
    assert 'heavy_worker_slots' not in launch
    assert parse({'enabled': True, 'heavy_worker_slots': 3})['heavy_worker_slots'] == 3
    unknown = deepcopy(DEFAULTS['profiles'])
    unknown['sol-xhigh'].pop('lightweight')
    with pytest.raises(ValueError, match='explicitly designate'):
        parse({'enabled': True, 'heavy_worker_slots': 3, 'profiles': unknown})
    for value in [True, 0, -1, '2']:
        with pytest.raises(ValueError, match='heavy_worker_slots'):
            parse({'enabled': True, 'heavy_worker_slots': value})


def test_execution_pipeline_is_opt_in_bounded_and_resume_preserves_recorded_policy():
    from trellis.dispatch.config import parse, resume_settings
    base = parse({'enabled': True})
    assert 'max_in_flight_tasks' not in base
    enabled = parse({'enabled': True, 'worker_slots': 5, 'max_in_flight_tasks': 8})
    assert enabled['max_in_flight_tasks'] == 8
    assert resume_settings(parse({'enabled': True, 'worker_slots': 5}), enabled,
                           {'enabled': True, 'worker_slots': 5}) == enabled
    for bad in (0, 4, True, '8'):
        with pytest.raises(ValueError, match='max_in_flight_tasks'):
            parse({'enabled': True, 'worker_slots': 5, 'max_in_flight_tasks': bad})
