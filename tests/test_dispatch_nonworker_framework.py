import json
import subprocess
import sys

import pytest

from test_dispatch_terminal_retention import setup, put, git
from test_dispatch_nonworker_retention import owner_setup
from trellis.dispatch import framework_retention as framework
from trellis.dispatch import terminal_retention as retention


@pytest.fixture
def framework_owner(owner_setup):
    collector, directory, observed, paths, state = owner_setup
    root = directory / 'repo/.trellis/runtime'
    for name in ('src/trellis/main.py', 'skills/test/SKILL.md', 'bin/kernel', 'runtime/staging/result.raw.json'):
        put(root / name, {'file': name})
    return collector, directory, observed, state, root


def test_typed_framework_drains_preserving_inputs_and_diagnostics(framework_owner):
    collector, directory, _, state, root = framework_owner
    observed = retention.terminal_owners(state, defer_verifiers=True)
    before = {p: p.read_bytes() for p in directory.rglob('*') if p.is_file() and '/src/' not in str(p) and '/skills/' not in str(p)}
    result = framework.retire(collector, directory, observed, apply=True)
    assert result['removed'] == 2
    assert all(p.read_bytes() == data for p, data in before.items())
    receipt = json.loads((directory / framework.RECEIPT).read_text())
    assert receipt['format'] == framework.OWNER_FORMAT
    assert receipt['owner']['kind'] == directory.name.split('-')[0]
    assert framework.retire(collector, directory, observed, apply=True)['status'] == 'absent'


@pytest.mark.parametrize('guard', ['active', 'uncommitted', 'pin', 'live', 'tracked', 'source', 'request'])
def test_typed_framework_guards(framework_owner, monkeypatch, guard):
    collector, directory, observed, state, root = framework_owner
    review = directory.name.startswith('review-')
    if guard == 'active':
        observed = {}
    elif guard == 'uncommitted':
        observed[directory.name] = 'another-owner'
    elif guard == 'pin':
        put(collector.root / 'reviews/2/retain-state.json' if review else directory / 'retain-builds.json', {})
    elif guard == 'live':
        put(directory / 'execution/process.json', {'pid': 123})
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: True)
    elif guard == 'tracked':
        git(directory, 'init', '-q')
        git(directory, 'add', 'repo/.trellis/runtime/src')
    else:
        prepared = json.loads((directory / 'prepared.json').read_text())
        prepared['source_repo' if guard == 'source' else 'request_id'] = str(directory) if guard == 'source' else 999
        put(directory / 'prepared.json', prepared)
    if guard in ('pin', 'live', 'source') or guard == 'request' and review:
        with pytest.raises(ValueError):
            framework.retire(collector, directory, observed, apply=True)
    else:
        result = framework.retire(collector, directory, observed, apply=True)
        assert result['removed'] == (1 if guard == 'tracked' else 2 if guard == 'request' else 0)
    if guard != 'request' or review:
        assert (root / 'src/trellis/main.py').exists()


def test_typed_framework_receipt_cannot_follow_reused_identity(framework_owner):
    collector, directory, observed, state, root = framework_owner
    framework.retire(collector, directory, observed, apply=True, budget={'files': 0})
    if directory.name.startswith('review-'):
        state['dispatch']['receipts']['2']['response_sha256'] = 'b' * 64
    else:
        state['dispatch']['verifiers']['jobs']['3']['identity']['panel_sha256'] = 'replacement'
    put(collector.repo / retention.HISTORY, {'state': state})
    git(collector.repo, 'add', '.')
    git(collector.repo, 'commit', '-qm', 'replace owner identity')
    with pytest.raises(ValueError, match='changed identity'):
        framework.retire(collector, directory, retention.terminal_owners(state), apply=True)
    assert (root / 'src/trellis/main.py').exists()


def test_typed_framework_cli(framework_owner):
    collector, directory, _, _, root = framework_owner
    result = subprocess.run([sys.executable, '-m', 'trellis.dispatch.framework_retention', '--runtime', str(collector.root.parent), '--repo', str(collector.repo), '--owner', directory.name, '--apply'], capture_output=True, text=True, check=True)
    assert json.loads(result.stdout)['removed'] == 2
    assert not (root / 'src').exists()


def test_background_collector_reaches_typed_framework(framework_owner):
    from trellis.dispatch.storage import BackgroundCollector
    original, directory, _, state, root = framework_owner
    collector = BackgroundCollector(original.root, repo=original.repo)
    collector._review_snapshots = None
    assert collector.schedule(state)
    assert collector.wait_idle(15)
    assert collector.summary['framework_trees'] == 2
    assert collector.summary['errors'] == 0
    assert not (root / 'src').exists()
