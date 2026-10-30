import copy
import json

import pytest

from trellis.checker import resource_failure as failure


def detail():
    return dict(operation='axioms', input_key='a'*64, policy_generation='0',
                attempt_count=3, reservation_bytes=16, limit_bytes=32,
                sampled_peak_bytes=30, cause='cgroup_oom', drained=True)


def test_capacity_is_distinct_and_first_cause_is_retained(tmp_path):
    failure.record_capacity(tmp_path, detail())
    original = (tmp_path / failure.CAPACITY_MARKER).read_bytes()
    failure.record_capacity(tmp_path, dict(detail(), operation='semantic'))
    assert (tmp_path / failure.CAPACITY_MARKER).read_bytes() == original
    with pytest.raises(failure.CapacityBlocked) as e:
        failure.require_healthy(tmp_path)
    assert e.value.record['detail'] == detail()
    assert not (tmp_path / 'checker-state/resource-failure.json').exists()


def test_existing_sticky_failure_takes_precedence(tmp_path):
    failure.record(tmp_path, 'unrelated transport failure')
    failure.record_capacity(tmp_path, detail())
    with pytest.raises(RuntimeError, match='unrelated transport failure') as e:
        failure.require_healthy(tmp_path)
    assert not isinstance(e.value, failure.CapacityBlocked)


@pytest.mark.parametrize('key,value', [('drained', False), ('cause', 'ordinary Lean error'),
                                      ('policy_generation', ''), ('attempt_count', True),
                                      ('reservation_bytes', -1), ('input_key', None)])
def test_incomplete_or_untyped_capacity_stop_fails_closed(tmp_path, key, value):
    value = dict(detail(), **{key:value})
    with pytest.raises(RuntimeError, match='invalid host capacity evidence'):
        failure.record_capacity(tmp_path, value)
    with pytest.raises(RuntimeError) as e:
        failure.require_healthy(tmp_path)
    assert not isinstance(e.value, failure.CapacityBlocked)


def test_runtime_binding_and_file_alias_are_not_capacity_authority(tmp_path):
    failure.record_capacity(tmp_path, detail())
    p=tmp_path/failure.CAPACITY_MARKER
    value=json.loads(p.read_text());value['runtime_root']=str(tmp_path/'other');p.write_text(json.dumps(value))
    with pytest.raises(RuntimeError, match='invalid host capacity evidence'):
        failure.require_healthy(tmp_path)


def test_known_capacity_rpc_error_is_not_generic_transport_failure(tmp_path, monkeypatch):
    monkeypatch.setenv(failure.HOST_RUNTIME_ENV,str(tmp_path))
    failure.record_transport_failure('capacity_blocked','retained')
    assert not (tmp_path/'checker-state/resource-failure.json').exists()
