"""Compact scheduling history from native frozen task bindings."""
from collections import Counter


RECENT_TASKS = 8


def by_node(tasks):
    """Count native closure launches once per task across the full ledger.

    A launch can precede provider startup. Corrections and transport segments
    retain the task ID; an ordinary native retry has a distinct task ID.
    Current profile configuration and read-only dependencies are irrelevant to
    historical model attribution and the node whose closure was attempted.
    """
    groups = {}
    for task in sorted(tasks.values(), key=lambda item: item['id']):
        assignment = task['assignment']
        if assignment['task_kind'] != 'lean_closure':
            continue
        binding = task.get('binding') or {}
        provider, model = binding.get('provider'), binding.get('model')
        for node in sorted(set(assignment['scope'].get('nodes', []))):
            key = (node, provider, model)
            if key not in groups:
                groups[key] = dict(provider=provider, model=model, profiles=set(), reasoning_efforts=set(),
                    launched_attempts=0, native_outcomes=Counter(), active_statuses=Counter(),
                    recent_launched_task_ids=[], unlaunched_reservations=[])
            group = groups[key]
            group['profiles'].add(assignment['profile'])
            if binding.get('reasoning_effort'):
                group['reasoning_efforts'].add(binding['reasoning_effort'])
            status = task['status']['status']
            if task.get('launched_at_ms') is None:
                if status == 'reserved':
                    group['unlaunched_reservations'].append(task['id'])
                continue
            group['launched_attempts'] += 1
            group['recent_launched_task_ids'] = (group['recent_launched_task_ids'] + [task['id']])[-RECENT_TASKS:]
            if status == 'resolved':
                group['native_outcomes'][task['status']['outcome']['kind']] += 1
            else:
                group['active_statuses'][status] += 1
    result = {}
    for (node, _, _), group in sorted(groups.items(), key=lambda item: tuple(part or '' for part in item[0])):
        group['profiles'] = sorted(group['profiles'])
        group['reasoning_efforts'] = sorted(group['reasoning_efforts'])
        group['native_outcomes'] = dict(sorted(group['native_outcomes'].items()))
        group['active_statuses'] = dict(sorted(group['active_statuses'].items()))
        result.setdefault(node, []).append(group)
    return result


def untried_candidates(candidates, history):
    """Current native candidates, before checking concurrent reservations."""
    return sorted(node for node in set(candidates)
                  if not any(row['launched_attempts'] for row in history.get(node, [])))
