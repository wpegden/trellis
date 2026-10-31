"""Native engineering adapters to existing allocation and retirement primitives."""
import hashlib
import json
import os
from pathlib import Path
import stat

from trellis.dispatch import retained_files
from trellis.dispatch.auxiliary_retention import _parents
from trellis.dispatch.candidate import canonical_json,_sync_dir
from trellis.dispatch.preparation_capacity import PreparationCapacity, PreparationDeferred, GIB
from trellis.dispatch.publication import atomic
from trellis.dispatch.storage import _attempt_directories

FORMAT = 'trellis-native-engineering-retirement/v1'


def capacity(root, runtime, **kwargs):
    return PreparationCapacity(runtime, root, floor_bytes=64 * GIB, **kwargs)


def admit_fresh(gate, root, state, current):
    """Only pre-owner allocation deferral is a normal engineering wait."""
    try:
        if not gate.ready():
            raise PreparationDeferred('storage preparation retry is pending')
        from trellis.modeling.engineering import delivery_allocation
        estimate = delivery_allocation(root, state, current)
        gate.require_allocation(estimate['bytes'] + gate.workspace_bytes,
                                estimate['inodes'] + gate.workspace_inodes)
    except PreparationDeferred as error:
        error.fresh_engineering_admission = True
        raise


def _raw(path):
    with retained_files.regular_path(path).open('rb') as stream:
        raw = stream.read(64 * 1024 * 1024 + 1)
    if len(raw) > 64 * 1024 * 1024:
        raise ValueError('Native retirement metadata exceeds its existing evidence bound')
    return raw


def _json(path):
    return json.loads(_raw(path))


def _directory_ids(paths):
    result = {}
    for path in paths:
        info = path.lstat()
        if not stat.S_ISDIR(info.st_mode):
            raise ValueError('Native retirement root is not an ordinary directory')
        result[str(path)] = [info.st_dev, info.st_ino, info.st_mode]
    return result


def _admitted_paths(delivery,attempt,home,work,checked):
    """Protect explicit inputs and non-WORK snapshots of executed admissions."""
    protected = {delivery/'request.json',delivery/'basis.json'}
    snapshots = []
    directory = home.parent/'engineering-admissions'
    if directory.is_symlink():
        raise ValueError('Native admissions directory is aliased')
    for index,path in enumerate((attempt/'runtime').iterdir()):
        if index >= 4096:raise ValueError('Native retained context inventory exceeds its existing bound')
        if not (path.name.startswith('engineering-session-') or path.name.startswith('engineering-interruption-')):
            continue
        receipt = _json(path)
        if (receipt['session']['home'] != str(home) or receipt['request_sha256']!=checked['request_sha256']
                or not 0<receipt['turn']<=checked['turn']):
            raise ValueError('Native retained home differs')
        from trellis.agent_wrapper import provider_clock
        from trellis.dispatch.process import alive
        clock=Path(receipt['execution_record']);finished=provider_clock.read(clock,'finished');exited=provider_clock.read(clock,'exited')
        if not finished or not exited or finished['owner']!=exited['owner'] or alive(finished['owner']):
            raise ValueError('Native retained recorder has not closed')
        required={str(clock.parent/'engineering-request.json'),*(str(clock/(name+'.json')) for name in ('config','started','finished','exited'))}
        if path.name.startswith('engineering-interruption-'):
            required.update(str(clock/(name+'.json')) for name in ('owner','session'))
        terminals=set(receipt['files'])-required
        if len(terminals)!=1:raise ValueError('Native retained launch evidence is ambiguous')
        launch=Path(terminals.pop())
        if launch.name!='launch.json' or launch.parent.suffix!='.execution':
            raise ValueError('Native retained launch path differs')
        admission_path=launch.parent.with_suffix('.json')
        if admission_path.parent!=directory:raise ValueError('Native retained admission is outside its home')
        raw=_raw(admission_path)
        if hashlib.sha256(raw).hexdigest()!=receipt['launch_identity']:
            raise ValueError('Native retained admission binding changed')
        from trellis.git_trust import read_engineering_receipt
        read_engineering_receipt(launch,receipt['launch_identity'],{'launcher_exited'})
        admission=json.loads(raw)
        if admission['home'] != str(home) or admission['work'] != str(work):
            raise ValueError('Native retained admission roots differ')
        saved=_json(Path(admission['request']['native_record']))
        if (saved['request']!=admission['request']['request']
                or saved['submission']['turn']!=receipt['turn']
                or saved['submission']['engineering_request']['id']!=checked['request_id']):
            raise ValueError('Native retained admission request differs')
        protected.update([path,admission_path,*map(Path,receipt['files']),*map(Path,admission['files']),*map(Path,admission['links'])])
        snapshots.extend(Path(tree['path']) for tree in admission['trees'] if tree['path'] != str(work))
    return protected,snapshots


def _leaves(scope,*,exclude=()):
    if not scope.exists():
        if scope.is_symlink():raise ValueError('Native retirement scope is aliased')
        return
    if scope.is_symlink() or not scope.is_dir():
        raise ValueError('Native retirement scope is not an ordinary directory')
    def failed(error):raise error
    for directory,dirs,names in os.walk(scope,followlinks=False,onerror=failed):
        dirs[:] = sorted(name for name in dirs if not (Path(directory)==scope and name in exclude))
        for name in dirs:
            path=Path(directory)/name
            if path.is_symlink() or name=='.git':
                raise ValueError('Native retirement scope has a directory alias or Git owner')
        for name in sorted(names):
            if Path(directory)==scope and name in exclude:continue
            path=Path(directory)/name;info=path.lstat()
            if not stat.S_ISREG(info.st_mode):
                raise ValueError('Native retirement leaf is aliased or special')
            yield path,info


def retire(root,*,runner_fd,apply=False,cancelled=lambda:False):
    """Retire only authenticated terminal leaves under the existing CLI locks."""
    from scripts import pv_modeling,paper_job_recovery,paper_launch_snapshot
    from trellis.modeling import engineering as eng,engineering_transport as transport,performance
    from trellis.modeling.driver import runtime_root
    from trellis.agent_wrapper.protocol import SingleAgentRequest
    from trellis.runtime.bridge import any_halt_marker_present
    root=Path(root);job=root.parent.parent;runtime=runtime_root(root)
    runner=root/'.trellis/modeling/runner.lock';control=job/'controller.lock'
    def ownership():
        retained_files.regular_path(control)
        held=os.fstat(runner_fd);current=runner.lstat()
        if ((held.st_dev,held.st_ino)!=(current.st_dev,current.st_ino)
                or not stat.S_ISREG(current.st_mode) or current.st_nlink!=1
                or pv_modeling.runner_pid(root)!=os.getpid() or pv_modeling._lock_pid(control)!=os.getpid()):
            raise ValueError('Native retirement requires its existing runner and controller ownership')
    ownership()
    if transport._OWNER.get() is not None:
        raise ValueError('Native retirement cannot run inside a request owner')
    snapshot=paper_launch_snapshot.validate_snapshot(job)
    if paper_job_recovery._owned_paths(job,snapshot['job'])[0]!=root:
        raise ValueError('Native retirement preparation differs from its immutable job')
    protocol_path=runtime/'protocol_state.json';protocol_raw=retained_files.regular_path(protocol_path).read_bytes()
    protocol=json.loads(protocol_raw);native=protocol['engineering']
    outer_ids=_directory_ids([root,runtime,job])
    if (protocol.get('in_flight_request') or protocol.get('worker_attempt') or native.get('completed')
            or native.get('halt') or native.get('failed') or native.get('handoff') or any_halt_marker_present(runtime)):
        return dict(status='not_idle',removed=0)
    transport._native_transport_no_pending(root);transport.native_execution_guard(root)
    transport._native_unresolved(root)
    if retained_files.regular_path(protocol_path).read_bytes()!=protocol_raw:
        raise ValueError('Native retirement validation changed protocol bytes')
    if native['subject']!=eng.native_subject(root):
        raise ValueError('Native retirement original inputs changed')
    def fence():
        ownership()
        if cancelled():return False
        if any_halt_marker_present(runtime):raise RuntimeError('Native retirement halted for operator review')
        if retained_files.regular_path(protocol_path).read_bytes()!=protocol_raw:
            raise ValueError('Native retirement protocol changed')
        if _directory_ids(map(Path,outer_ids))!=outer_ids:
            raise ValueError('Native retirement outer root changed')
        transport._native_transport_no_pending(root)
        return True
    free_before=os.statvfs(root).f_bavail*os.statvfs(root).f_frsize
    result=dict(status='eligible',removed=0,removed_bytes=0,removed_allocated_bytes=0,retained_files=0,assignments=[])
    result.update(eligible_bytes=0,eligible_unique_inode_allocated_bytes=0)
    def finish():
        status=os.statvfs(root)
        result.update(free_bytes_before=free_before,free_bytes_after=status.f_bavail*status.f_frsize)
        return result
    for cursor,checked,host in eng._native_consumptions(root,native):
        if not fence():result['status']='cancelled';break
        delivery=root/host['attempt'];turn=Path(host['turn_path']);attempt=turn.parent.parent
        complete=delivery/'build-caches-pruned.json';inventory_path=delivery/'storage-retirement-inventory.json'
        identity=dict(request_sha256=checked['request_sha256'],checked_sha256=cursor['checked_sha256'],
                      consumption_sha256=eng.digest(cursor),delivery=str(delivery),attempt=str(attempt))
        if complete.exists() or complete.is_symlink():
            done=_json(complete)
            if (done.get('format')!=FORMAT or done.get('identity')!=identity or done.get('completed') is not True
                    or retained_files._digest_file(inventory_path)!=done.get('inventory_sha256')):
                raise ValueError('Native retirement completion identity differs')
            continue  # A completed receipt never authorizes deleting recreated files.
        pin_roots=[delivery,*_attempt_directories(attempt)]
        pins=[path/name for path in pin_roots for name in ('retain-builds.json','retain-state.json')]
        if any(path.exists() or path.is_symlink() for path in pins):
            result['assignments'].append(dict(delivery=str(delivery),status='pinned'));continue
        transport._native_consumed_turns(root,delivery,host,checked)
        delivery_signatures={path:retained_files._signature(retained_files.regular_path(path).stat())
                             for path in (delivery/'request.json',delivery/'basis.json')}
        sealed=eng.history._verified(turn/'candidate')[0][1]
        ordinal=sealed['metadata'].get('transport_ordinal',0)
        from trellis.dispatch.worker_transport import execution_directory
        saved=_json(execution_directory(turn,ordinal)/'engineering-request.json')
        single=SingleAgentRequest.from_dict(saved['request']);home=single.burst_home;work=delivery/'view'
        if home.parent!=delivery/'burst-homes' or single.work_dir!=work:
            raise ValueError('Native retirement home or view is outside its delivery')
        directories=_directory_ids([delivery,attempt,home,home/'.codex',work,work/'.trellis'])
        pins.extend(path/name for path in (home,work) for name in ('retain-builds.json','retain-state.json'))
        if any(path.exists() or path.is_symlink() for path in pins):
            result['assignments'].append(dict(delivery=str(delivery),status='pinned'));continue
        execution_guard=retained_files._execution_guard(attempt,pins=pins)
        protected,trees=_admitted_paths(delivery,attempt,home,work,checked)
        scopes=[(home/'target','cache',None)]
        scratch_copies={}
        runs={}
        outcome=checked['outcome']
        if outcome['outcome']=='implementation':
            for check in outcome['candidate']['checks']:
                record=eng.native_read(root,check['receipt_sha256'])
                if performance.recorded_evidence(root,record['id'])!=record:
                    raise ValueError('Native Cargo retirement evidence differs')
                runs[record['id']]=record
        elif outcome['outcome']=='measure':
            _,record=eng._native_measurement(root,outcome['measurement']);runs[record['id']]=record
        for run,record in runs.items():
            directory=root/performance.STORE/'runs'/run
            record_path=retained_files.regular_path(directory/'record.json')
            record_signature=list(retained_files._signature(record_path.stat()))
            if (retained_files._digest_file(record_path)!=record['record_sha256']
                    or list(retained_files._signature(record_path.stat()))!=record_signature):
                raise ValueError('Native retirement run record changed before pinning')
            protected.update([directory/'record.json',directory/'record.sha256',*(directory/name for name in record['evidence_files'])])
            protected.update(Path(command['executable']['external_path']) for command in record['commands']
                             if command.get('executable',{}).get('external_path'))
            # Generated hashes do not retain bytes. Only a declared extant
            # artifact with identical bytes and full mode covers scratch.
            copies={}
            for name,digest in record['evidence_files'].items():
                if name.startswith('artifacts/'):
                    copies.setdefault(digest,[]).append((directory/name,record['evidence_modes'][name]))
            generated=_json(directory/'generated-final.json')
            for name,item in generated.items():
                if not name.startswith(('build/','output/')) or set(item)!={'sha256','size'}:continue
                path=directory/'workspace'/eng.history._relative(name)
                if not path.exists() and not path.is_symlink():continue
                info=path.lstat()
                if stat.S_ISREG(info.st_mode) and info.st_nlink!=1:
                    _parents(path,root)
                    continue
                info=retained_files.regular_path(path).stat()
                for copy,mode in copies.get(item['sha256'],()):
                    if info.st_size==item['size'] and stat.S_IMODE(info.st_mode)==mode:
                        if retained_files._digest_file(path)!=item['sha256']:
                            raise ValueError('Native retirement scratch bytes differ from retained observations')
                        copy_info=retained_files.regular_path(copy).stat();copy_signature=list(retained_files._signature(copy_info))
                        if (stat.S_IMODE(copy_info.st_mode)!=mode or retained_files._digest_file(copy)!=item['sha256']
                                or list(retained_files._signature(copy.stat()))!=copy_signature):
                            raise ValueError('Native retirement observation changed before pinning')
                        scratch_copies[path]=dict(path=str(copy),sha256=item['sha256'],mode=mode,
                            run_id=run,record_sha256=record['record_sha256'],record_path=str(record_path),
                            record_signature=record_signature,signature=copy_signature)
                        break
            scopes.extend((directory/'workspace'/name,'scratch',None) for name in ('build','output'))
        _,seal=eng.history._verified(turn/'candidate')[0]
        covered={entry['path'][5:]:(entry['sha256'],seal['metadata']['modes'][entry['path'][5:]])
                 for entry in seal['files'] if entry['path'].startswith('view/')}
        scopes.append((work,'view',covered))
        protected.update(trees)
        def pinned(path):return path in protected or any(parent in protected for parent in path.parents)
        if inventory_path.exists() or inventory_path.is_symlink():
            record=_json(inventory_path)
            if record.get('format')!=FORMAT or record.get('identity')!=identity or record.get('directories')!=directories:
                raise ValueError('Native retirement inventory identity differs')
        else:
            entries=[]
            for scope,kind,coverage in scopes:
                if scope.exists() and not retained_files._untracked(scope):
                    continue
                for path,info in _leaves(scope,exclude={'.trellis','_CHATS'} if kind=='view' else ()):
                    if info.st_nlink!=1 or pinned(path):
                        result['retained_files']+=1;continue
                    proof=None
                    if kind=='scratch' and path not in scratch_copies:
                        result['retained_files']+=1;continue
                    if coverage is not None:
                        name=path.relative_to(work).as_posix();proof=coverage.get(name)
                        if proof is None or proof!=(retained_files._digest_file(path),stat.S_IMODE(info.st_mode)):
                            result['retained_files']+=1;continue
                    entries.append(dict(path=path.relative_to(root).as_posix(),kind=kind,
                        signature=list(retained_files._signature(info)),mode=info.st_mode,bytes=info.st_size,
                        allocated_bytes=info.st_blocks*512,parents=_parents(path,root),proof=list(proof) if proof else None,
                        retained_copy=scratch_copies.get(path),
                        blob_signature=(list(retained_files._signature(retained_files.regular_path(turn/'candidate/blobs'/proof[0]).stat()))
                                        if proof else None)))
            record=dict(format=FORMAT,identity=identity,directories=directories,files=entries)
        seen=set();ready=[]
        for entry in record['files']:
            relative=eng.history._relative(entry['path']);path=root/relative
            if relative in seen:raise ValueError('Native retirement inventory has duplicate paths')
            seen.add(relative)
            matches=[(scope,kind,coverage) for scope,kind,coverage in scopes if path.is_relative_to(scope)]
            if len(matches)!=1 or entry['kind']!=matches[0][1] or pinned(path):
                raise ValueError('Native retirement inventory escaped its exact scopes')
            scope,kind,coverage=matches[0]
            if kind=='view' and (path.relative_to(work).parts[0] in {'.trellis','_CHATS'}
                    or list(coverage.get(path.relative_to(work).as_posix(),()))!=entry['proof']):
                raise ValueError('Native retirement view proof differs')
            if kind=='scratch' and scratch_copies.get(path)!=entry.get('retained_copy'):
                if path.exists() or path.is_symlink():raise ValueError('Native retirement retained scratch copy differs')
            if not path.exists() and not path.is_symlink():continue
            info=retained_files.regular_path(path).stat()
            if (_parents(path,root)!=entry['parents'] or list(retained_files._signature(info))!=entry['signature']
                    or info.st_mode!=entry['mode'] or info.st_size!=entry['bytes']
                    or info.st_blocks*512!=entry['allocated_bytes']):
                raise ValueError('Native retirement leaf changed')
            if kind=='view' and (retained_files._digest_file(path)!=entry['proof'][0]
                    or stat.S_IMODE(info.st_mode)!=entry['proof'][1]):
                raise ValueError('Native retirement view bytes differ')
            ready.append((path,entry,scope))
        result['eligible_bytes']+=sum(entry['bytes'] for _,entry,_ in ready)
        result['eligible_unique_inode_allocated_bytes']+=sum(entry['allocated_bytes'] for _,entry,_ in ready)
        result['assignments'].append(dict(delivery=str(delivery),files=len(record['files'])))
        if not apply:continue
        if not fence():result['status']='cancelled';break
        if not retained_files._untracked(inventory_path) or not retained_files._untracked(complete):
            raise ValueError('Native retirement controls became Git tracked')
        if not inventory_path.exists():
            record['free_bytes_before']=free_before
            raw=canonical_json(record)
            if len(raw)>64*1024*1024:raise ValueError('Native retirement inventory exceeds its evidence bound')
            atomic(inventory_path,raw)
        if retained_files.regular_path(inventory_path).read_bytes()!=canonical_json(record):
            raise ValueError('Native retirement published inventory changed')
        inventory_signature=retained_files._signature(retained_files.regular_path(inventory_path).stat())
        dirty=set()
        try:
            transport._native_unresolved(root)
            transport._native_consumed_turns(root,delivery,host,checked)
            execution_guard()
            if _directory_ids(map(Path,directories))!=directories:
                raise ValueError('Native retirement root identity changed')
            active_scope=None
            for path,entry,scope in ready:
                if any_halt_marker_present(runtime):raise RuntimeError('Native retirement halted for operator review')
                if (os.path.lexists(delivery/'execution-failure.json') or any(
                        retained_files._signature(retained_files.regular_path(control).stat())!=expected
                        for control,expected in delivery_signatures.items())):
                    raise RuntimeError('Native retirement delivery evidence changed')
                if cancelled() or any(path.exists() or path.is_symlink() for path in pins):
                    result['status']='cancelled' if cancelled() else 'pinned';return finish()
                if scope!=active_scope:
                    if not fence():result['status']='cancelled';return finish()
                    if not retained_files._untracked(scope):raise ValueError('Native retirement scope became Git tracked')
                    execution_guard();active_scope=scope
                if retained_files._signature(retained_files.regular_path(inventory_path).stat())!=inventory_signature:
                    raise ValueError('Native retirement inventory changed')
                if not path.exists() and not path.is_symlink():continue
                info=retained_files.regular_path(path).stat()
                if (_parents(path,root)!=entry['parents'] or list(retained_files._signature(info))!=entry['signature']
                        or info.st_mode!=entry['mode'] or info.st_size!=entry['bytes']
                        or info.st_blocks*512!=entry['allocated_bytes']):
                    raise ValueError('Native retirement leaf changed')
                if entry['proof'] and list(retained_files._signature(retained_files.regular_path(
                        turn/'candidate/blobs'/entry['proof'][0]).stat()))!=entry['blob_signature']:
                    raise ValueError('Native retirement recovery blob changed')
                if entry.get('retained_copy'):
                    copy=entry['retained_copy'];info_copy=retained_files.regular_path(copy['path']).stat()
                    if (list(retained_files._signature(info_copy))!=copy['signature']
                            or stat.S_IMODE(info_copy.st_mode)!=copy['mode']
                            or list(retained_files._signature(retained_files.regular_path(copy['record_path']).stat()))!=copy['record_signature']):
                        raise ValueError('Native retirement retained observation changed')
                path.unlink();dirty.add(path.parent)
                result['removed']+=1;result['removed_bytes']+=entry['bytes'];result['removed_allocated_bytes']+=entry['allocated_bytes']
        finally:
            for parent in sorted(dirty):_sync_dir(parent)
        if not fence():result['status']='cancelled';break
        execution_guard()
        transport._native_unresolved(root)
        transport._native_consumed_turns(root,delivery,host,checked)
        if not fence():result['status']='cancelled';break
        for run,expected in runs.items():
            if performance.recorded_evidence(root,run)!=expected:
                raise ValueError('Native measurement evidence changed during retirement')
        if any((root/entry['path']).exists() or (root/entry['path']).is_symlink() for entry in record['files']):
            result['status']='inputs_reappeared';continue
        atomic(complete,canonical_json(dict(format=FORMAT,identity=identity,completed=True,
            inventory_sha256=retained_files._digest_file(inventory_path),free_bytes_after=os.statvfs(root).f_bavail*os.statvfs(root).f_frsize)))
    return finish()
