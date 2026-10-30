"""Fresh proc reads retain baseline results, ordering and error behavior."""
import builtins
import errno
import importlib.util
import io
import os
from pathlib import Path

import pytest
from trellis import memory_guard as mod

_SPEC = importlib.util.spec_from_file_location('memory_poll_baseline',
    Path(__file__).parent/'fixtures/memory_proc_table_85fa308b.py')
_BASE = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(_BASE)
old = _BASE._read_proc_table


def process(root, pid, parent, pages, comm='lean (worker)'):
    p = root/str(pid); p.mkdir()
    (p/'stat').write_text(f'{pid} ({comm}) S {parent} 0 0 0\n')
    (p/'statm').write_text(f'100 {pages} 0 0\n')
    return p


def test_table_and_descendants_preserve_order_comm_rss_and_cycles(tmp_path, monkeypatch):
    process(tmp_path, 10, 0, 2)
    process(tmp_path, 11, 10, 7, 'private ) name (')
    process(tmp_path, 12, 11, 3)
    process(tmp_path, 20, 21, 1)
    process(tmp_path, 21, 20, 1)
    process(tmp_path, '١', 0, 1)
    (tmp_path/'nonnumeric').mkdir()
    process(tmp_path, 30, 'bad-parent', 10)
    (process(tmp_path, 31, 10, 9)/'statm').write_text('bad')
    expected = old(tmp_path)
    assert mod._read_proc_table(tmp_path) == expected
    assert expected[11] == (10, 7*mod._PAGE, 'private ) name (')
    fresh = mod._read_proc_table
    monkeypatch.setattr(mod, '_read_proc_table', old)
    baseline = [mod.descendant_rss(pid, tmp_path) for pid in [10,20,99]]
    monkeypatch.setattr(mod, '_read_proc_table', fresh)
    assert [mod.descendant_rss(pid,tmp_path) for pid in [10,20,99]] == baseline
    assert baseline[0][0] == 12*mod._PAGE


@pytest.mark.parametrize('error', [errno.EACCES, errno.EIO, errno.ENOENT])
@pytest.mark.parametrize('failed_file', ['stat','statm'])
def test_read_errors_skip_exact_same_process(tmp_path, monkeypatch, error, failed_file):
    process(tmp_path, 10, 0, 2); process(tmp_path, 11, 10, 3)
    original = builtins.open
    def read(path,*args,**kwargs):
        if Path(path)==tmp_path/'10'/failed_file: raise OSError(error,'injected')
        return original(path,*args,**kwargs)
    monkeypatch.setattr(builtins,'open',read); monkeypatch.setattr(io,'open',read)
    assert mod._read_proc_table(tmp_path)==old(tmp_path)=={11:(10,3*mod._PAGE,'lean (worker)')}


def test_listing_error_is_still_empty(tmp_path, monkeypatch):
    def failed(*args): raise OSError(errno.EIO,'injected')
    monkeypatch.setattr(os,'listdir',failed)
    assert mod._read_proc_table(tmp_path)==old(tmp_path)=={}


def test_invalid_text_is_not_silently_swallowed(tmp_path):
    p=process(tmp_path,10,0,2);(p/'stat').write_bytes(b'\xff')
    for read in [old,mod._read_proc_table]:
        with pytest.raises(UnicodeDecodeError):read(tmp_path)


def test_each_poll_rereads_both_files_in_same_order(tmp_path, monkeypatch):
    process(tmp_path,10,0,2);process(tmp_path,11,10,3)
    original=builtins.open;calls=[]
    def read(path,*args,**kwargs):
        calls.append(os.fspath(path));return original(path,*args,**kwargs)
    monkeypatch.setattr(builtins,'open',read);monkeypatch.setattr(io,'open',read)
    expected=old(tmp_path);old_calls=list(calls);calls.clear()
    assert mod._read_proc_table(tmp_path)==expected
    assert calls==old_calls
    with original(tmp_path/'11/statm','w') as f:f.write('100 23 0\n')
    assert mod._read_proc_table(tmp_path)==old(tmp_path)
    assert mod._read_proc_table(tmp_path)[11][1]==23*mod._PAGE


def test_existing_symlink_read_semantics_unchanged(tmp_path):
    p=process(tmp_path,10,0,2);target=tmp_path/'data';target.write_text('100 9 0\n')
    (p/'statm').unlink();(p/'statm').symlink_to(target)
    assert mod._read_proc_table(tmp_path)==old(tmp_path)=={10:(0,9*mod._PAGE,'lean (worker)')}


def test_memory_cap_and_host_floor_reasons_identical(tmp_path, monkeypatch):
    process(tmp_path,10,0,2);process(tmp_path,11,10,3)
    mem=tmp_path/'meminfo';mem.write_text('MemAvailable: 1 kB\n')
    new=mod._read_proc_table
    for kwargs in [dict(limit_bytes=mod._PAGE),dict(limit_bytes=0,floor_bytes=1<<30,floor_min_tree_bytes=1)]:
        monkeypatch.setattr(mod,'_read_proc_table',old)
        a=mod.MemoryGuard(proc_root=tmp_path,meminfo=mem,**kwargs);expected=a.check(10)
        monkeypatch.setattr(mod,'_read_proc_table',new)
        b=mod.MemoryGuard(proc_root=tmp_path,meminfo=mem,**kwargs)
        assert b.check(10)==expected
        assert b.peak_bytes==a.peak_bytes and b.rows_at_peak==a.rows_at_peak
