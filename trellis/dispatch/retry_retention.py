"""Discard consumed retry preparations after committed terminal custody ends.

NativeLedger.prepare_retries reads these files only for pending predecessors.
No acknowledgement or replacement archive is needed once the exact successor
or refusal is committed and both current/Git states have ended that custody.
"""
from . import retained_files as retained, terminal_retention as terminal
from .auxiliary_retention import _binding, _parents
from .candidate import _sync_dir


def _integer(value):
    return type(value) is int and value > 0


def observations(state):
    tasks = state['dispatch']['tasks']
    children = {}
    for key, task in tasks.items():
        predecessor = task.get('retry_of')
        if _integer(predecessor):
            children.setdefault(str(predecessor), []).append((key, task))
    result = {}
    for key, prior in tasks.items():
        if (not _integer(prior.get('id')) or key != str(prior['id'])
                or prior.get('status', {}).get('status') != 'resolved'
                or prior.get('pending_retry') is not None):
            continue
        owned = {key: terminal.task_identity(prior)}
        successors = children.get(key, [])
        refusal = prior.get('retry_refusal')
        if not successors and isinstance(refusal, str) and refusal.strip():
            result[key] = dict(predecessor=prior['id'], owners=owned, task=None, refusal=refusal)
        elif len(successors) == 1 and refusal is None:
            child_key, child = successors[0]
            assignment = child.get('assignment', {})
            if (not _integer(child.get('id')) or child_key != str(child['id'])
                    or child.get('status', {}).get('status') != 'resolved'
                    or child.get('pending_retry') is not None
                    or child.get('review_id') != prior.get('review_id')
                    or type(child.get('reserved_at_ms')) is not int or child['reserved_at_ms'] < 0
                    or not isinstance(assignment.get('assignment_id'), str)
                    or not isinstance(assignment.get('base_snapshot'), str)):
                continue
            owned[child_key] = terminal.task_identity(child)
            result[key] = dict(predecessor=prior['id'], owners=owned, refusal=None,
                task=dict(id=child['id'], now_ms=child['reserved_at_ms'],
                          assignment_id=assignment['assignment_id'], base_snapshot=assignment['base_snapshot']))
    return result


class Collector:
    def __init__(self, collector, admission=None):
        self.collector,self.admission=collector,admission

    def retire(self,predecessor,current,*,apply=False):
        from .storage import _attempt_directories
        c=self.collector
        result=dict(status='ineligible',removed=0,removed_bytes=0,removed_allocated_bytes=0)
        if not _integer(predecessor):raise ValueError('invalid retry predecessor')
        expected=current()
        if expected is None or c.cancelled():return result
        path=c.root/'retries'/str(predecessor)/'prepared.json'
        if not path.exists() and not path.is_symlink():return dict(result,status='absent')
        signature=retained._signature(retained.regular_path(path).stat());parents=_parents(path,c.root)
        if self.admission is not None:self.admission()
        witness=c.witness()
        if witness.get('retry_preparations',{}).get(str(predecessor))!=expected:
            return dict(result,status='not_committed')
        if any(witness['owners'].get(key)!=identity for key,identity in expected['owners'].items()):
            return dict(result,status='not_committed')
        owners=[c.root/key for key in expected['owners']]
        guards=[retained.terminal_owner_guard(c.root,owner,expected['owners']) for owner in owners]
        bindings={owner:_binding(owner,c,witness,dict(kind='worker',id=int(owner.name))) for owner in owners}
        pins=[attempt/'retain-builds.json' for owner in owners for attempt in _attempt_directories(owner)]
        pins += [path.parent/'retain-builds.json',path.parent/'retain-state.json']
        # The sole consumer selects only pending predecessors. Once current
        # and committed custody has ended, this fixed derived cache is unused
        # regardless of its historical payload/schema; never parse it again.
        if not retained._untracked(path):return dict(result,status='tracked')
        if not apply:return dict(result,status='eligible')
        with c.retirement_lock() as acquired:
            if not acquired:return dict(result,status='coordinator_busy')
            if c.cancelled():return dict(result,status='cancelled')
            if self.admission is not None:self.admission()
            if current()!=expected or not c.checkpoint_current(witness):
                raise ValueError('retry custody or checkpoint changed')
            if any(c._same_task_pending(int(owner.name)) for owner in owners):
                raise ValueError('retry owner has pending publication')
            for guard in guards:guard()
            if any(pin.exists() or pin.is_symlink() for pin in pins):raise ValueError('retry preparation is pinned')
            for owner,binding in bindings.items():
                if list(retained._signature(retained.regular_path(owner/'prepared.json').stat()))!=binding['prepared_signature']:
                    raise ValueError('retry owner preparation changed')
            if (_parents(path,c.root)!=parents or retained._signature(retained.regular_path(path).stat())!=signature
                    or not retained._untracked(path)):
                raise ValueError('retry preparation changed before unlink')
            info=path.stat();path.unlink();_sync_dir(path.parent)
            result.update(status='retired',removed=1,removed_bytes=info.st_size,removed_allocated_bytes=info.st_blocks*512)
        return result
