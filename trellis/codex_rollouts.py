"""Logical Codex session files, including losslessly retained terminal history.

Native Codex CLI still requires ordinary JSONL paths. These helpers are only
for Trellis history/timing/usage readers; restore raw files before CLI resume.
"""
from pathlib import Path


def logical_path(path):
    path = Path(path)
    name = path.name
    for suffix in ('.retained.json', '.gz'):
        if name.endswith(suffix):
            name = name[:-len(suffix)]
            break
    if not name.startswith('rollout-') or not name.endswith('.jsonl'):
        return None
    return path.with_name(name)


def discover(root):
    """Deduplicate raw/archive names while retaining incomplete archive hints."""
    return sorted({logical for pattern in ('rollout-*.jsonl', 'rollout-*.jsonl.gz',
                                          'rollout-*.jsonl.retained.json')
                   for path in Path(root).rglob(pattern)
                   if (logical := logical_path(path)) is not None})


def read_text(path):
    # Existing ordinary history may have hardlinks; its read semantics remain
    # unchanged. Only archived representation uses the independent-file checks.
    path = Path(path)
    try:
        return path.read_text(encoding='utf-8', errors='replace')
    except FileNotFoundError:
        from trellis.dispatch import retained_files as retained
        return retained.read_bytes(path).decode('utf-8', errors='replace')


def original_mtime_ns(path):
    """Stable latest-session ordering; never substitute archive creation time."""
    path = Path(path)
    try:
        return path.stat().st_mtime_ns
    except FileNotFoundError:
        from trellis.dispatch import retained_files as retained
        _, record = retained._record(path)
        if 'mtime_ns' not in record:
            raise ValueError('retained session has no original mtime; restore raw session before usage selection')
        return record['mtime_ns']


def _find_usage(obj):
    """Depth-first hunt for the innermost dict carrying input_tokens."""
    if isinstance(obj, dict):
        if "input_tokens" in obj and isinstance(obj.get("input_tokens"), int):
            return dict(obj)
        for value in obj.values():
            got = _find_usage(value)
            if got:
                return got
    elif isinstance(obj, list):
        for value in obj:
            got = _find_usage(value)
            if got:
                return got
    return {}


def rollout_usage_fallback(codex_home, thread_id):
    """Last usage snapshot from the session rollout file under ``<codex_home>/.codex``.

    A burst killed at the wall never emits `turn.completed`, so its stream
    carries no usage — and those are the longest, most expensive attempts,
    the ones whose cost most needs counting. The rollout file still records
    the usage the session had reached.
    """
    import json
    if not codex_home:
        return {}
    sessions = Path(codex_home) / ".codex" / "sessions"
    if not sessions.is_dir():
        return {}
    # discover() gives a deterministic logical-path tie break for equal original
    # timestamps; archive creation time never changes the chosen session.
    candidates = sorted(discover(sessions), key=original_mtime_ns)
    if thread_id:
        matched = [p for p in candidates if thread_id in p.name]
        if matched:
            candidates = matched
    if not candidates:
        return {}
    best = {}
    for raw in read_text(candidates[-1]).splitlines():
        if "input_tokens" not in raw:
            continue
        try:
            parsed = json.loads(raw)
        except ValueError:
            continue
        found = _find_usage(parsed)
        if found:
            best = found
    return best
