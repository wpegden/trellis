import copy

from trellis.dispatch.closure_history import by_node, untried_candidates


def task(i, *, model='light-model', provider='openai', profile='light',
         node='N', launched=True, status='resolved', outcome='stuck'):
    state = dict(status=status)
    if status == 'resolved':
        state['outcome'] = dict(kind=outcome)
    return dict(id=i, binding=dict(provider=provider, model=model, reasoning_effort='high'),
                assignment=dict(task_kind='lean_closure', profile=profile,
                                scope=dict(nodes=[node], readonly_nodes=['Dependency'])),
                launched_at_ms=i if launched else None, status=state)


def test_full_ledger_history_uses_frozen_models_and_native_outcomes():
    tasks = {str(i): task(i) for i in range(1, 31)}
    tasks['31'] = task(31, profile='renamed-light', outcome='accepted')
    tasks['32'] = task(32, model='replacement-light', outcome='stale')
    tasks['33'] = task(33, provider='other-provider', outcome='timed_out')
    tasks['34'] = task(34, status='checking')
    tasks['35'] = task(35, launched=False, status='reserved')
    tasks['36'] = task(36, node='Ordinary')
    tasks['36']['assignment']['task_kind'] = 'ordinary'
    original = copy.deepcopy(tasks)

    history = by_node(tasks)
    assert set(history) == {'N'}
    first, replacement, other = history['N']
    assert first == dict(provider='openai', model='light-model',
        profiles=['light', 'renamed-light'], reasoning_efforts=['high'],
        launched_attempts=32, native_outcomes={'accepted': 1, 'stuck': 30},
        active_statuses={'checking': 1}, recent_launched_task_ids=[25, 26, 27, 28, 29, 30, 31, 34],
        unlaunched_reservations=[35])
    assert replacement['model'] == 'replacement-light'
    assert replacement['native_outcomes'] == {'stale': 1}
    assert other['provider'] == 'other-provider'
    assert other['native_outcomes'] == {'timed_out': 1}
    assert tasks == original


def test_retry_tasks_count_once_and_transports_and_corrections_stay_in_task():
    original = task(1, status='reserved')
    original['worker_attempt'] = dict(turn=3, transport_retries=2, submissions=2)
    retry = task(2, status='running')
    retry['retry_of'] = 1
    history = by_node({'1': original, '2': retry})['N'][0]
    assert history['launched_attempts'] == 2
    assert history['active_statuses'] == {'reserved': 1, 'running': 1}
    assert history['native_outcomes'] == {}


def test_unlaunched_and_unknown_bindings_do_not_invent_attempts_or_models():
    reserved = task(1, node='Reserved', launched=False, status='reserved')
    cancelled = task(2, node='Cancelled', launched=False, outcome='cancelled')
    unknown = task(3, node='Unknown')
    del unknown['binding']
    unknown['worker_binding'] = dict(model='fallback-heavy')
    history = by_node({'1': reserved, '2': cancelled, '3': unknown})
    assert history['Unknown'][0]['model'] is None
    assert history['Unknown'][0]['provider'] is None
    assert history['Reserved'][0]['unlaunched_reservations'] == [1]
    assert history['Cancelled'][0]['native_outcomes'] == {}
    assert untried_candidates(['Unknown', 'Reserved', 'Cancelled', 'New', 'New'], history) == [
        'Cancelled', 'New', 'Reserved']


def test_candidate_list_keeps_native_eligibility_and_all_models_count():
    history = by_node({'1': task(1, model='heavy-model', node='HeavyTried')})
    assert untried_candidates(['HeavyTried', 'Eligible'], history) == ['Eligible']
    assert untried_candidates([], history) == []
