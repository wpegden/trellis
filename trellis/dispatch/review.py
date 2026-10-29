"""Parallel reviewer envelope; each task retains the ordinary review contract."""
from __future__ import annotations

import argparse
import json
import re
from pathlib import Path


def evidence_paths(value):
    """Exact evidence names; this parser grants no source authority itself."""
    if not isinstance(value, list) or len(value) > 64:
        raise ValueError('evidence_paths must be a list of at most 64 exact files')
    if any(not isinstance(path, str) or not path.startswith('reference/model-evidence/')
           or not re.fullmatch(r'[A-Za-z0-9_./-]+', path)
           or any(part in ('', '.', '..') for part in path.split('/')) for path in value):
        raise ValueError('evidence_paths requires normalized file paths under reference/model-evidence/')
    if len(set(value)) != len(value):
        raise ValueError('evidence_paths cannot repeat a file')
    return sorted(value)


def normalize(raw, request, *, normalize_review=None):
    if normalize_review is None:
        from trellis.checking import normalize_trellis_reviewer_result_data
        normalize_review = normalize_trellis_reviewer_result_data
    if not isinstance(raw, dict) or set(raw) - {'assignments', 'action', 'reason', 'system_feedback'}:
        raise ValueError('parallel review requires assignments, optional action, reason and system_feedback')
    items = raw.get('assignments', [])
    if not isinstance(items, list) or len(items) > 64:
        raise ValueError('assignments must be a list of at most 64 tasks; fill only available slots')
    if raw.get('action') is not None and items:
        raise ValueError('submit either worker assignments or one ordinary reviewer action')
    menu = request['parallel_dispatch']['profiles']
    assignments, seen = [], set()
    for index, item in enumerate(items):
        error = None
        id = item.get('id', f'item-{index + 1}') if isinstance(item, dict) else f'item-{index + 1}'
        normalized, evidence = None, []
        try:
            if not isinstance(item, dict) or set(item) - {'id', 'profile', 'timeout_seconds', 'compiler', 'task_kind', 'review', 'evidence_paths'}:
                raise ValueError('assignment requires id, profile, compiler, task_kind and an ordinary review object')
            if not isinstance(id, str) or not re.fullmatch(r'[A-Za-z0-9_-]{1,96}', id) or id in seen:
                raise ValueError('assignment id must be unique and use letters, digits, underscores or hyphens')
            seen.add(id)
            profile = menu.get(item.get('profile'))
            if profile is None:
                raise ValueError('profile is not available in this run')
            if item.get('compiler') not in ('none', 'managed'):
                raise ValueError('compiler must be none or managed')
            if item.get('task_kind') not in profile['allowed_tasks']:
                raise ValueError('task_kind is not permitted by this model profile')
            evidence = evidence_paths(item.get('evidence_paths', []))
            if evidence and (request.get('phase') not in ('modeling', 'Modeling')
                             or request.get('model_preservation_enabled') is not True
                             or item['task_kind'] != 'ordinary' or item['compiler'] != 'managed'):
                raise ValueError('evidence_paths requires ordinary Modeling with model preservation and compiler=managed')
            timeout = item.get('timeout_seconds')
            if timeout is not None and (type(timeout) is not int or not 0 < timeout <= profile['max_timeout_seconds']):
                raise ValueError('timeout_seconds exceeds the selected profile limit')
            result = normalize_review(item.get('review'), review_request=request, worker_assignment=True,
                                      assignment_task_kind=item['task_kind'])
            normalized = result.get('response')
            if not isinstance(normalized, dict):
                raise ValueError('; '.join(result.get('errors', [])) or 'ordinary assignment review is invalid')
            if normalized.get('assumption_authoring_request') and item['compiler'] != 'managed':
                raise ValueError('assumption-authoring assignments require compiler=managed')
        except (ValueError, TypeError) as exc:
            error = str(exc)
        assignments.append(dict(id=id if isinstance(id, str) else f'item-{index + 1}',
            profile=item.get('profile', '') if isinstance(item, dict) else '',
            timeout_seconds=item.get('timeout_seconds') if isinstance(item, dict) else None,
            compiler=item.get('compiler', 'managed') if isinstance(item, dict) else 'managed',
            task_kind=item.get('task_kind', 'ordinary') if isinstance(item, dict) else 'ordinary',
            review=normalized, preparation_error=error))
        if evidence:
            assignments[-1]['evidence_paths'] = evidence
    action = None
    if raw.get('action') is not None:
        result = normalize_review(raw['action'], review_request=request)
        action = result.get('response')
        if not isinstance(action, dict):
            raise ValueError('; '.join(result.get('errors', [])) or 'ordinary reviewer action is invalid')
        if action.get('dispatch_node_retirement'):
            raise ValueError('node retirement dispatches a worker: put dispatch_node_retirement in an '
                             'ordinary managed assignment review, not action; choose a permitted active '
                             'coarse goal and include the retired nodes and authorized consumers in scope')
        if action.get('assumption_authoring_request'):
            raise ValueError('assumption authoring dispatches a worker: put assumption_authoring_request '
                             'in an ordinary managed assignment review, not action; wait for current '
                             'tasks to finish and submit it alone, with routing and scope fields empty')
    return dict(kind='parallel_review', assignments=assignments, action=action,
                reason=str(raw.get('reason', '')), system_feedback=str(raw.get('system_feedback', '')))



def validate(raw, request):
    """Use the bridge's normalizer for the advertised reviewer check too."""
    try:
        result = normalize(raw, request)
        errors = [f"{item['id']}: {item['preparation_error']}"
                  for item in result['assignments'] if item['preparation_error']]
        assignments = [item for item in result['assignments'] if not item['preparation_error']]
        if assignments:
            # Advisory only: stale frozen scopes must never become bridge
            # preparation errors or suppress current native partial admission.
            from trellis.checking import _kernel_response
            required = ('phase', 'current_present_nodes', 'current_deps')
            if any(key not in request for key in required) or 'tasks' not in request['parallel_dispatch']:
                raise ValueError('parallel scope check requires phase, current_present_nodes, current_deps and parallel_dispatch.tasks')
            checked = _kernel_response(dict(action='check_dispatch_scopes', input=dict(
                **{key: request[key] for key in required}, active_node=request.get('active_node'),
                tasks=[dict(**{key: task[key] for key in ('id', 'status', 'scope')},
                            task_kind=task.get('task_kind', 'ordinary'))
                       for task in request['parallel_dispatch']['tasks']],
                assignments=[dict(id=item['id'], task_kind=item['task_kind'],
                    **({'evidence_paths': item['evidence_paths']} if item.get('evidence_paths') else {}), review={key: item['review'].get(key)
                    for key in ('next_active', 'next_mode', 'authorized_nodes')}) for item in assignments])),
                expected_status='dispatch_ok')
            if checked['ok']:
                errors.extend(f'{id}: {error}' for id, error in checked['data']['errors'].items())
            else:
                errors.extend(checked['errors'])
        return dict(ok=not errors, errors=errors, data=result)
    except (ValueError, TypeError, KeyError) as error:
        return dict(ok=False, errors=[str(error)], data=None)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--context', type=Path, required=True)
    parser.add_argument('--raw', type=Path, required=True)
    args = parser.parse_args()
    try:
        result = validate(json.loads(args.raw.read_text()), json.loads(args.context.read_text()))
        print(json.dumps(dict(ok=result['ok'], errors=result['errors'])))
        return int(not result['ok'])
    except (ValueError, OSError) as exc:
        print(json.dumps(dict(ok=False, errors=[str(exc)])))
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
