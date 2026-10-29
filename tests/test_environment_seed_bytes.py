import json

import pytest

from trellis.checker import portable_replay as portable
from trellis.dispatch import acceptance_receipts, retained_files


def fixture(repo, *, pretty=False, version=None):
    value = dict(version=portable.VERSION if version is None else version,
                 text='λ exact environment ' * 100)
    data = portable._json(value)
    identity = portable.digest(value)
    path = repo / portable.DIRECTORY / (identity + '.json')
    path.parent.mkdir(parents=True)
    path.write_bytes(json.dumps(value, indent=2, ensure_ascii=False).encode() if pretty else data)
    return value, data, identity, path


@pytest.mark.parametrize('representation', ['raw', 'pretty', 'archived'])
def test_exact_validated_bytes_and_existing_decoded_api(tmp_path, monkeypatch, representation):
    value, data, identity, path = fixture(tmp_path, pretty=representation == 'pretty')
    if representation == 'archived':
        monkeypatch.setattr(retained_files, 'MIN_BYTES', 0)
        monkeypatch.setattr(retained_files, '_untracked', lambda path: True)
        assert retained_files.archive(path)
        assert not path.exists()
    calls = []
    original = portable._json
    monkeypatch.setattr(portable, '_json', lambda value: (calls.append(1), original(value))[1])
    assert portable.environment_bytes_for(tmp_path, identity) == data
    assert calls == [1]
    assert portable.environment_for(tmp_path, identity) == value


def test_budget_exact_boundary_and_early_rejection(tmp_path, monkeypatch):
    _, data, identity, _ = fixture(tmp_path)
    assert portable.environment_bytes_for(tmp_path, identity, max_bytes=len(data)) == data
    monkeypatch.setattr(acceptance_receipts, 'regular', lambda path: pytest.fail('oversized optional candidate read'))
    with pytest.raises(portable.Unavailable, match='oversized'):
        portable.environment_bytes_for(tmp_path, identity, max_bytes=len(data) - 1)


def test_pretty_optional_copy_can_stay_cold_without_changing_decoded_reader(tmp_path):
    value, data, identity, path = fixture(tmp_path, pretty=True)
    assert path.stat().st_size > len(data)
    with pytest.raises(portable.Unavailable, match='oversized'):
        portable.environment_bytes_for(tmp_path, identity, max_bytes=len(data))
    assert portable.environment_for(tmp_path, identity) == value


def test_raced_size_metadata_cannot_bypass_actual_bound(tmp_path, monkeypatch):
    _, data, identity, _ = fixture(tmp_path)
    monkeypatch.setattr(retained_files, 'logical_size', lambda path: 1)
    with pytest.raises(portable.Unavailable, match='oversized'):
        portable.environment_bytes_for(tmp_path, identity, max_bytes=len(data) - 1)


def test_original_decoded_api_keeps_input_size_limit_semantics(tmp_path, monkeypatch):
    raw = ('{"version":%d,"number":1e9}' % portable.VERSION).encode()
    value = json.loads(raw)
    identity = portable.digest(value)
    path = tmp_path / portable.DIRECTORY / (identity + '.json')
    path.parent.mkdir(parents=True)
    path.write_bytes(raw)
    assert len(portable._json(value)) > len(raw)
    monkeypatch.setattr(portable, 'MAX_MANIFEST_BYTES', len(raw))
    assert portable.environment_for(tmp_path, identity) == value
    with pytest.raises(portable.Unavailable, match='oversized'):
        portable.environment_bytes_for(tmp_path, identity, max_bytes=len(raw))


def test_wrong_version_does_not_serialize(tmp_path, monkeypatch):
    _, _, identity, _ = fixture(tmp_path, version=-1)
    monkeypatch.setattr(portable, '_json', lambda value: pytest.fail('unsupported version serialized'))
    with pytest.raises(portable.Unavailable, match='changed'):
        portable.environment_for(tmp_path, identity)


@pytest.mark.parametrize('change', ['bytes', 'version', 'identity', 'symlink'])
def test_each_call_revalidates_and_rejects_changed_environment(tmp_path, change):
    value, data, identity, path = fixture(tmp_path)
    assert portable.environment_bytes_for(tmp_path, identity) == data
    if change == 'bytes':
        path.write_bytes(portable._json(dict(value, text='changed')))
    elif change == 'version':
        value['version'] = -1
        identity = portable.digest(value)
        path = path.with_name(identity + '.json')
        path.write_bytes(portable._json(value))
    elif change == 'identity':
        identity = '0' * 64
        path.with_name(identity + '.json').write_bytes(data)
    else:
        outside = tmp_path / 'outside.json'
        path.rename(outside)
        path.symlink_to(outside)
    with pytest.raises((ValueError, OSError)):
        portable.environment_bytes_for(tmp_path, identity)
