"""Bounded JSON projection for obsolete, potentially gigabyte-sized batch captures.

Validate the complete JSON syntax but keep only receipt and worker-reference
metadata. Source strings and private states are never materialized. Limits are
parser memory/shape limits, not a retirement work budget.
"""
import json
import re

_CHUNK = 65536
_MAX_TOKEN = 1 << 20
_MAX_DEPTH = 128
_MAX_KEYS = 100000
_MAX_METADATA = 8 << 20
_STRING_RUN = re.compile(r'[^"\\\x00-\x1f]*')
_NUMBER = re.compile(r'-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?\Z')


class Projection:
    def __init__(self, stream, *, cancelled=lambda: False):
        self.stream, self.cancelled = stream, cancelled
        self.buffer, self.offset, self.eof = '', 0, False
        self.kept_bytes, self.live_keys = 0, 0

    def fill(self):
        if self.cancelled():
            raise InterruptedError('batch projection cancelled')
        if self.offset == len(self.buffer) and not self.eof:
            self.buffer = self.stream.read(_CHUNK)
            self.offset = 0
            self.eof = not self.buffer
        return self.offset < len(self.buffer)

    def peek(self):
        return self.buffer[self.offset] if self.fill() else ''

    def take(self):
        c = self.peek()
        if not c:
            raise ValueError('truncated batch JSON')
        self.offset += 1
        return c

    def space(self):
        while self.peek() and self.peek() in ' \t\r\n':
            self.offset += 1

    def string(self, keep):
        if self.take() != '"':
            raise ValueError('expected JSON string')
        parts, size = [], 0
        while True:
            if not self.fill():
                raise ValueError('unterminated batch JSON string')
            end = _STRING_RUN.match(self.buffer, self.offset).end()
            if keep:
                part = self.buffer[self.offset:end]
                size += len(part)
                if size > _MAX_TOKEN:
                    raise ValueError('oversized batch metadata string')
                parts.append(part)
            self.offset = end
            if end == len(self.buffer):
                continue
            c = self.take()
            if c == '"':
                return json.loads('"' + ''.join(parts) + '"') if keep else None
            if c != '\\':
                raise ValueError('control byte in JSON string')
            escape = self.take()
            if escape == 'u':
                tail = ''.join(self.take() for _ in range(4))
                if not re.fullmatch('[0-9a-fA-F]{4}', tail):
                    raise ValueError('invalid unicode escape')
                escape += tail
            elif escape not in '"\\/bfnrt':
                raise ValueError('invalid JSON escape')
            if keep:
                parts.append('\\' + escape)
                size += len(escape) + 1
                if size > _MAX_TOKEN:
                    raise ValueError('oversized batch metadata string')

    @staticmethod
    def keep(path):
        if not path:
            return True
        if path[0] in {'base_sha256', 'build_cache_mode', 'format', 'receipt', 'task_ids', 'tasks', 'objects', 'checkpoint'}:
            return True
        if path[0] != 'prepared':
            return False
        if len(path) <= 1:
            return True
        if path[1] in {'format', 'receipt'}:
            return True
        if path[1] == 'event':
            return len(path) == 2 or path[2] in {'kind', 'review_id', 'response_sha256', 'now_ms'}
        if path[1] != 'workers':
            return False
        return len(path) <= 3 or path[3] in {'format', 'sha256', 'bytes'}

    @staticmethod
    def marker(path):
        # Retain bounded key presence at every schema boundary. Discarded
        # payloads remain None, so strict validators still reject unknown fields.
        return (len(path) == 1 or (path[:1] == ('prepared',) and len(path) == 2)
                or (path[:2] == ('prepared', 'event') and len(path) == 3)
                or (path[:2] == ('prepared', 'workers') and len(path) == 4))

    def value(self, path=(), depth=0):
        if depth > _MAX_DEPTH:
            raise ValueError('batch JSON nesting exceeds supported depth')
        self.space()
        c, keep = self.peek(), self.keep(path)
        if keep:
            self.kept_bytes += 64
            if self.kept_bytes > _MAX_METADATA:
                raise ValueError('batch projected metadata exceeds memory bound')
        if c == '{':
            self.take(); self.space(); result, seen, key_bytes = {}, set(), 0
            if self.peek() == '}':
                self.take(); return result if keep else None
            while True:
                self.space(); key = self.string(True)
                if key in seen or len(seen) >= _MAX_KEYS:
                    raise ValueError('duplicate or excessive batch JSON object keys')
                cost = len(key) * 4 + 64
                key_bytes += cost; self.live_keys += cost
                if self.live_keys > _MAX_METADATA:
                    raise ValueError('batch JSON keys exceed memory bound')
                seen.add(key); self.space()
                if self.take() != ':':
                    raise ValueError('missing JSON colon')
                child_path = path + (key,)
                child = self.value(child_path, depth + 1)
                if keep and (self.keep(child_path) or self.marker(child_path)):
                    self.kept_bytes += cost
                    if self.kept_bytes > _MAX_METADATA:
                        raise ValueError('batch projected metadata exceeds memory bound')
                    result[key] = child
                self.space(); separator = self.take()
                if separator == '}':
                    self.live_keys -= key_bytes
                    return result if keep else None
                if separator != ',':
                    raise ValueError('invalid JSON object separator')
        if c == '[':
            self.take(); self.space(); result, count = [], 0
            if self.peek() == ']':
                self.take(); return result if keep else None
            while True:
                value = self.value(path + (str(count),), depth + 1)
                if keep:
                    if count >= _MAX_KEYS:
                        raise ValueError('excessive batch metadata array')
                    result.append(value)
                count += 1; self.space(); separator = self.take()
                if separator == ']':
                    return result if keep else None
                if separator != ',':
                    raise ValueError('invalid JSON array separator')
        if c == '"':
            value = self.string(keep)
            if keep:
                self.kept_bytes += len(value) * 4
                if self.kept_bytes > _MAX_METADATA:
                    raise ValueError('batch projected metadata exceeds memory bound')
            return value
        token = []
        while self.peek() and self.peek() not in ' \t\r\n,]}':
            token.append(self.take())
            if len(token) > _MAX_TOKEN:
                raise ValueError('oversized JSON scalar')
        raw = ''.join(token)
        if raw not in {'true', 'false', 'null'} and not _NUMBER.fullmatch(raw):
            raise ValueError('invalid batch JSON scalar')
        return json.loads(raw) if keep else None

    def read(self):
        result = self.value()
        self.space()
        if self.peek() or not isinstance(result, dict):
            raise ValueError('invalid batch JSON root or trailing data')
        return result


def read(stream, *, cancelled=lambda: False):
    return Projection(stream, cancelled=cancelled).read()
