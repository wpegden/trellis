"""Configuration is shared within one checkpoint and refreshed by the next."""
import json
from pathlib import Path
import subprocess

from trellis.runtime import git_checkpoint_hook as hook
from test_engineering_checkpoint_guard import checkpoint
from test_git_checkpoint_hook_trust_decisions import _git, _init_repo, _payload
from test_paper_engineering import root


SOURCE = Path(__file__).resolve().parents[1]


def configuration(repo):
    value = json.loads((SOURCE / 'examples/trellis.config.json').read_bytes())
    value['repo_path'] = str(repo)
    value['history']['shared_state'] = False
    value['git'].update(author_name='Initial Fixture', author_email='fixture@example.invalid', remote_name='local-fixture')
    return value


def save(path, value):
    path.write_text(json.dumps(value) + '\n')


def count_parses(monkeypatch):
    original, calls = hook.load_config, []
    def load(path):
        calls.append(path)
        return original(path)
    monkeypatch.setattr(hook, 'load_config', load)
    return calls


def local_archive(tmp_path):
    remote = tmp_path / 'local-archive.git'
    subprocess.run(['git', 'init', '--bare', str(remote)], check=True, capture_output=True)
    return remote


def assert_mirrored(repo, remote, commit):
    branch = _git(repo, 'branch', '--show-current').strip()
    archived = subprocess.check_output(['git', '--git-dir', str(remote), 'rev-parse', 'refs/heads/' + branch], text=True).strip()
    assert archived == commit == _git(repo, 'rev-parse', 'HEAD').strip()


def test_direct_checkpoints_reload_committed_configuration_once(tmp_path, monkeypatch):
    repo = _init_repo(tmp_path)
    path = repo / 'trellis.config.json'; config = configuration(repo); save(path, config)
    payload = _payload(repo, tmp_path); payload['metadata']['config_path'] = str(path)
    calls = count_parses(monkeypatch)
    first = hook.commit_checkpoint(payload)
    assert len(calls) == 1
    assert '$format' not in json.loads(_git(repo, 'show', 'HEAD:.trellis-history/supervisor_state.json'))
    remote = local_archive(tmp_path)
    config['git'].update(remote_url=str(remote), author_name='Updated Fixture')
    config['history']['shared_state'] = True
    save(path, config)
    _git(repo, 'add', 'trellis.config.json'); _git(repo, 'commit', '-qm', 'Commit synthetic archive configuration')
    payload['event_count'] += 1
    second = hook.commit_checkpoint(payload)
    assert len(calls) == 2
    assert second != first and _git(repo, 'show', '-s', '--format=%an', 'HEAD').strip() == 'Updated Fixture'
    assert json.loads(_git(repo, 'show', 'HEAD:.trellis-history/supervisor_state.json'))['$format'] == 'trellis-shared-state/1'
    assert_mirrored(repo, remote, second)


def test_invalid_configuration_is_cached_only_within_its_checkpoint(tmp_path, monkeypatch):
    repo = _init_repo(tmp_path)
    path = repo / 'trellis.config.json'; path.write_text('{')
    payload = _payload(repo, tmp_path); payload['metadata']['config_path'] = str(path)
    calls = count_parses(monkeypatch)
    hook.commit_checkpoint(payload)
    assert len(calls) == 1
    remote = local_archive(tmp_path)
    config = configuration(repo); config['git']['remote_url'] = str(remote); save(path, config)
    payload['event_count'] += 1
    second = hook.commit_checkpoint(payload)
    assert len(calls) == 2
    assert_mirrored(repo, remote, second)


def test_committed_native_retry_does_not_parse_or_push(checkpoint, monkeypatch):
    repo, payload, view = checkpoint
    path = repo / 'trellis.config.json'; save(path, configuration(repo))
    payload['metadata']['config_path'] = str(path)
    committed = hook.commit_checkpoint(payload)
    path.write_text('{')
    view.rename(view.with_name('retained-view'))
    def forbidden(*args, **kwargs):
        raise AssertionError('durably committed retry attempted configuration parsing or publication')
    monkeypatch.setattr(hook, 'load_config', forbidden)
    monkeypatch.setattr(hook, '_maybe_push_to_archive', forbidden)
    assert hook.commit_checkpoint(payload) == committed
    assert _git(repo, 'rev-parse', 'HEAD').strip() == committed
