import json
from types import SimpleNamespace

import pytest

from trellis.adapters import ProviderConfig
from trellis.agents.codex_tool_mode import direct_tool_catalog


@pytest.fixture(autouse=True)
def confined_export(monkeypatch):
    monkeypatch.setenv('TRELLIS_COMPILER_DATA_VIEW', '/fixture/noexec')
    def wrap(command, **kwargs):
        assert kwargs['sandbox'].enabled and kwargs['sandbox'].backend == 'bwrap'
        assert kwargs['role'] in ('worker', 'reviewer')
        assert kwargs['git_preflight_path']
        return ['confined-export', *command]
    monkeypatch.setattr('trellis.agents.codex_tool_mode.wrap_command', wrap)


def test_direct_catalog_preserves_model_metadata_and_caller_configuration(tmp_path, monkeypatch):
    catalog = {'models': [dict(slug='sol', tool_mode='code_mode_only', context_window=123,
                               model_messages={'instructions_template': 'existing prose'})]}
    seen = []
    def export(command, **kwargs):
        seen.append(command)
        assert kwargs['env']['PATH']
        assert kwargs['cwd'] == tmp_path
        return SimpleNamespace(returncode=0, stdout=json.dumps(catalog))
    monkeypatch.setattr('trellis.agents.codex_tool_mode.subprocess.run', export)
    config = ProviderConfig('codex', model='sol', extra_args=['-p', 'custom', '-c', 'model_catalog_json="/custom.json"', '--help'])
    path = direct_tool_catalog(config, tmp_path, tmp_path, role='reviewer')
    copied = json.loads(path.read_text())
    assert copied['models'][0].pop('tool_mode') == 'direct'
    original = dict(catalog['models'][0]); original.pop('tool_mode')
    assert copied['models'] == [original]
    assert seen == [['confined-export', 'codex', '-p', 'custom', 'debug', 'models', '-c', 'model_catalog_json="/custom.json"']]
    assert catalog['models'][0]['tool_mode'] == 'code_mode_only'


@pytest.mark.parametrize('output', ['{}', '{"models": []}', '{"models": [{"slug": "other"}]}', 'invalid'])
def test_direct_catalog_refuses_incomplete_provider_metadata(tmp_path, monkeypatch, output):
    monkeypatch.setattr('trellis.agents.codex_tool_mode.subprocess.run',
        lambda *a, **kw: SimpleNamespace(returncode=0, stdout=output))
    with pytest.raises(RuntimeError, match='Cannot prepare Codex direct tools'):
        direct_tool_catalog(ProviderConfig('codex', model='sol'), tmp_path, tmp_path)
    assert not (tmp_path / '.codex/trellis-direct-models.json').exists()


def test_catalog_export_failure_preserves_provider_diagnostic(tmp_path, monkeypatch):
    monkeypatch.setattr('trellis.agents.codex_tool_mode.subprocess.run', lambda *a, **kw:
        SimpleNamespace(returncode=2, stdout='', stderr='unknown configuration profile'))
    with pytest.raises(RuntimeError, match=r'catalog export failed \(exit 2\): unknown configuration profile'):
        direct_tool_catalog(ProviderConfig('codex', model='sol'), tmp_path, tmp_path)


def test_catalog_refuses_export_without_compiler_view(tmp_path, monkeypatch):
    monkeypatch.delenv('TRELLIS_COMPILER_DATA_VIEW')
    monkeypatch.setattr('trellis.agents.codex_tool_mode.subprocess.run',
                        lambda *a, **kw: pytest.fail('unconfined metadata discovery'))
    monkeypatch.delenv('TRELLIS_COMPILER_BWRAP', raising=False)
    with pytest.raises(RuntimeError, match='compiler confinement is required'):
        direct_tool_catalog(ProviderConfig('codex', model='sol'), tmp_path, tmp_path)


def test_catalog_refuses_unguarded_role(tmp_path, monkeypatch):
    monkeypatch.setattr('trellis.agents.codex_tool_mode.subprocess.run',
                        lambda *a, **kw: pytest.fail('unguarded metadata discovery'))
    with pytest.raises(RuntimeError, match='compiler confinement does not support role'):
        direct_tool_catalog(ProviderConfig('codex', model='sol'), tmp_path, tmp_path, role='unknown')


@pytest.mark.parametrize('artifact', ['catalog', 'instructions'])
@pytest.mark.parametrize('link', ['file', 'directory', 'home'])
def test_host_files_never_follow_agent_symlinks(tmp_path, monkeypatch, artifact, link):
    from trellis.agents.codex_headless import _materialize_base_instructions
    external = tmp_path / 'host'; external.mkdir()
    sentinel = external / 'sentinel'; sentinel.write_text('preserve host bytes')
    home = tmp_path / 'home'; home.mkdir()
    directory = home / '.codex'; directory.mkdir()
    name = 'trellis-direct-models.json' if artifact == 'catalog' else 'trellis-worker-base-instructions.md'
    if link == 'file':
        (directory / name).symlink_to(sentinel)
    elif link == 'directory':
        directory.rmdir(); directory.symlink_to(external, target_is_directory=True)
    else:
        directory.rmdir(); home.rmdir(); home.symlink_to(external, target_is_directory=True)
    exports = []
    def export(*a, **kw):
        exports.append(True)
        return SimpleNamespace(returncode=0, stdout='{"models":[{"slug":"sol"}]}')
    monkeypatch.setattr('trellis.agents.codex_tool_mode.subprocess.run', export)
    def prepare():
        if artifact == 'catalog':
            return direct_tool_catalog(ProviderConfig('codex', model='sol'), home, tmp_path)
        return _materialize_base_instructions(home, 'worker')
    if link == 'file':
        path = prepare()
        assert path == directory / name and not path.is_symlink()
        assert path.read_text() != 'preserve host bytes'
    else:
        with pytest.raises(RuntimeError, match='Cannot prepare Codex'):
            prepare()
        assert not exports, 'reject symlink parent before loading provider configuration'
    assert sentinel.read_text() == 'preserve host bytes'
    assert sorted(p.name for p in external.iterdir()) == ['sentinel']


def test_host_file_parent_replacement_cannot_redirect_write(tmp_path):
    from trellis.agents.host_files import atomic_text, provider_directory
    home = tmp_path / 'home'; home.mkdir()
    external = tmp_path / 'host'; external.mkdir()
    sentinel = external / 'output'; sentinel.write_text('preserve')
    with provider_directory(home) as (directory, fd):
        directory.rename(home / 'previous')
        directory.symlink_to(external, target_is_directory=True)
        atomic_text(directory, fd, 'output', 'generated')
    assert sentinel.read_text() == 'preserve'
    assert (home / 'previous/output').read_text() == 'generated'
