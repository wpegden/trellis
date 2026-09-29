"""Canonical git-tracked supervisor artifact paths, and the shared-state codec.

The second half of this module implements ``trellis-shared-state/1``, the
structural-sharing + string-interning container format for
``.trellis-history/supervisor_state.json``.  See ``SHARED_STATE_SPEC`` below for
the normative description; golden test vectors live in
``tests/fixtures/shared_state_vectors/``.
"""

from __future__ import annotations

import copy
import hashlib
import json
from pathlib import Path
from typing import Any, Dict, List, Tuple


PROJECT_HISTORY_DIRNAME = ".trellis-history"
WORKER_STATE_DIRNAME = "worker_state"

SUPERVISOR_STATE_FILENAME = "supervisor_state.json"
WORKER_HANDOFF_FILENAME = "worker_handoff.json"
PAPER_RESULT_FILENAME = "paper_faithfulness_result.json"
CORR_RESULT_FILENAME = "correspondence_result.json"
SOUND_RESULT_FILENAME = "soundness_result.json"
REVIEW_RESULT_FILENAME = "reviewer_decision.json"
LAST_INVALID_DIRNAME = "last_invalid"
LAST_INVALID_METADATA_FILENAME = "metadata.json"


def project_history_dir(repo_path: Path) -> Path:
    return repo_path / PROJECT_HISTORY_DIRNAME


def supervisor_state_path(repo_path: Path) -> Path:
    return project_history_dir(repo_path) / SUPERVISOR_STATE_FILENAME


def worker_handoff_path(repo_path: Path) -> Path:
    return project_history_dir(repo_path) / WORKER_HANDOFF_FILENAME


def paper_result_path(repo_path: Path) -> Path:
    return project_history_dir(repo_path) / PAPER_RESULT_FILENAME


def corr_result_path(repo_path: Path) -> Path:
    return project_history_dir(repo_path) / CORR_RESULT_FILENAME


def sound_result_path(repo_path: Path) -> Path:
    return project_history_dir(repo_path) / SOUND_RESULT_FILENAME


def review_result_path(repo_path: Path) -> Path:
    return project_history_dir(repo_path) / REVIEW_RESULT_FILENAME


def worker_state_dir(repo_path: Path) -> Path:
    return project_history_dir(repo_path) / WORKER_STATE_DIRNAME


def last_invalid_dir(repo_path: Path) -> Path:
    return worker_state_dir(repo_path) / LAST_INVALID_DIRNAME


def last_invalid_tablet_dir(repo_path: Path) -> Path:
    return last_invalid_dir(repo_path) / "Tablet"


def last_invalid_metadata_path(repo_path: Path) -> Path:
    return last_invalid_dir(repo_path) / LAST_INVALID_METADATA_FILENAME


# ---------------------------------------------------------------------------
# trellis-shared-state/1 codec
# ---------------------------------------------------------------------------

SHARED_STATE_FORMAT = "trellis-shared-state/1"

#: Top-level keys whose values are encoded.  Everything else at the top level
#: is passed through verbatim so that consumers reading only ``event_count`` /
#: ``metadata`` / ``commands`` (``kernel/src/bin/runtime_cli.rs::segment_event_log``,
#: the first phase of ``scripts/prepare_migrated_resume.sh``) need no change.
SHARED_STATE_ENCODED_KEYS: Tuple[str, ...] = ("checkpoint", "state")

#: Number of leading hex characters of the SHA-256 Merkle digest used as a
#: table key.  Fixed forever for format version 1.
SHARED_STATE_KEY_LEN = 16

#: Serialized byte cost of one reference, ``{"$r":"<key>"}`` / ``{"$s":"<key>"}``
#: rendered with compact separators.
_REF_COST = 9 + SHARED_STATE_KEY_LEN

#: Serialized byte cost of one ``$pool`` entry's own framing -- ``"<key>":``
#: plus the trailing comma -- excluding the stored body.
_POOL_ENTRY_OVERHEAD = SHARED_STATE_KEY_LEN + 4

#: Same for one ``$strings`` entry, whose body carries two more quotes.
_STRING_ENTRY_OVERHEAD = SHARED_STATE_KEY_LEN + 6

_SIGIL_KEYS = frozenset({"$r", "$s", "$e"})

SHARED_STATE_SPEC = """\
trellis-shared-state/1
======================

A container format for a JSON *object* document.  Two designated top-level
values (`checkpoint`, `state`) are rewritten into a shared-structure form; every
other top-level key is carried through unchanged.

Document shape
--------------
    {
      "$format": "trellis-shared-state/1",
      <passthrough keys, in the input document's own order>,
      "$strings": { "<key>": "<string>", ... },   # sorted by key
      "$pool":    { "<key>": <encoded node>, ... },# sorted by key
      "checkpoint": <encoded node>,               # only if present in input
      "state":      <encoded node>                # only if present in input
    }

`$strings` and `$pool` are always present (possibly empty).  The pool spans both
encoded subtrees, so a subtree shared between `checkpoint` and `state` is stored
once.

Detection is purely structural: a document is in this format iff it is a JSON
object with a `"$format"` member.  A `"$format"` whose value is not exactly
`"trellis-shared-state/1"` is an error, never a fallback to passthrough.

Encoded nodes
-------------
Encoding is a recursive function `enc(v)`:

  * `null`, booleans, numbers                  -> unchanged
  * string  s, interned                        -> {"$s": "<key>"}
  * string  s, not interned                    -> s
  * array   a                                  -> [enc(e) for e in a],
                                                  then pooling (below)
  * object  o                                  -> {k: enc(o[k])}, members emitted
                                                  sorted by UTF-8 byte order of k,
                                                  then sigil-escape, then pooling

Sigil escape: if the encoded object has exactly one member and that member's key
is `"$r"`, `"$s"` or `"$e"`, it is wrapped as `{"$e": <that object>}`.  This is
applied before pooling and makes the three sigils unambiguous for any input.

Pooling: if a container's plain form was selected for the pool, the encoded
container body is stored once as `$pool["<key>"]` and every occurrence is
replaced by `{"$r": "<key>"}`.

Decoding is the exact inverse.  An object is a reference iff it has exactly one
member:
  * `{"$r": k}` -> a *fresh deep copy* of `dec($pool[k])`
  * `{"$s": k}` -> `$strings[k]`
  * `{"$e": o}` -> the object `{kk: dec(o[kk])}` (the inner object is a literal;
                   its own single-member sigil shape is NOT re-interpreted)
Any other object decodes memberwise.  Decoding never shares mutable objects
between two expansions of the same key.

Merkle digest
-------------
Every JSON value has a 32-byte SHA-256 Merkle digest `H(v)`.  `hex(v)` is its
64-character lowercase hex encoding, as ASCII bytes.

    H(null)      = SHA256(b"n")
    H(true)      = SHA256(b"t")
    H(false)     = SHA256(b"f")
    H(int i)     = SHA256(b"i" + ascii(decimal(i)))
    H(float f)   = SHA256(b"d" + ascii(shortest_roundtrip(f)))
    H(string s)  = SHA256(b"s" + utf8(s))
    H(array a)   = SHA256(b"a" + hex(a[0]) + hex(a[1]) + ...)
    H(object o)  = SHA256(b"o" + hex(k0) + hex(o[k0]) + hex(k1) + hex(o[k1]) + ...)
                   with keys k0 < k1 < ... by UTF-8 byte order, and hex(k) the
                   digest of the key taken as a *string* value.

Integers and floats are distinct: `1` and `1.0` have different digests.
`shortest_roundtrip(f)` is Python's `repr(float)`: the shortest decimal string
that parses back to the same IEEE-754 double, always containing `.` or `e`,
keeping the sign of `-0.0`, and writing exponents with a sign and at least two
digits (`1e-09`, `1e+16`).  Non-finite floats are rejected.

Digests are computed only where values are encoded, and this module is the only
encoder; the Rust and JavaScript implementations decode and never hash.  An
encoder in another language must therefore reproduce `repr` exactly, which
shortest-roundtrip formatters generally do not: `String(f)` in JavaScript yields
`0` for `-0.0`, `1e-9` for `1e-09`, and the expanded `0.00001` and
`10000000000000000` for `1e-05` and `1e+16`.  Such an encoder in JavaScript
would also need a number-type-preserving parser, since JavaScript cannot
distinguish `1` from `1.0` after parsing.

Table keys
----------
A table key is the first 16 characters of `hex(v)`.  Two distinct values that
truncate to the same key are a hard error at encode time (the encoder raises;
it never silently overwrites).

Selection
---------
Let `count(v)` be the number of occurrences of values with digest `H(v)` across
the full traversal of the encoded subtrees (each occurrence counted once, object
keys excluded), and let `weight(v)` be a deterministic size proxy:

    weight(null) = 4 ; weight(true) = 4 ; weight(false) = 5
    weight(int)  = len(decimal(i))
    weight(float)= len(shortest_roundtrip(f))
    weight(str)  = 2 + len(utf8(s))
    weight(arr)  = 2 + sum(weight(e)) + max(0, len(arr) - 1)
    weight(obj)  = 2 + sum(3 + len(utf8(k)) + weight(o[k])) + max(0, len(obj) - 1)

A string is interned iff

    count * weight  >  count * 25  +  (16 + 6 + len(utf8(s)))

A container (array or object) is pooled iff

    count * weight  >  count * 25  +  (20 + weight)

25 is the byte cost of one reference and 16/20 the per-entry table overhead for
`SHARED_STATE_KEY_LEN == 16`.  Both inequalities are strict.  Selection is done
on the *plain* values, so it never depends on the encoding of descendants.

Emission order
--------------
`$strings` and `$pool` members are emitted in ascending UTF-8 byte order of
their keys (they are lowercase hex, so plain ASCII order).  Object members
inside encoded subtrees are emitted in ascending UTF-8 byte order of their keys.
Passthrough top-level members keep the input document's order.  This makes the
serialized form a deterministic function of the parsed input document.

Errors
------
The decoder raises `SharedStateError` on: an unknown `$format`; a missing or
non-object `$strings`/`$pool`; a top-level `$`-prefixed member other than
`$format`, `$strings` and `$pool` (an extension must bump `$format`); a
`$strings`/`$pool` member present without `$format`; a `$r`/`$s` whose value is
not a string; a key absent from the corresponding table; a `$strings` member
that is not a string; a `$e` whose value is not an object; and a `$r` cycle.  It never substitutes a default.  A well-formed encoder output is
acyclic by construction (a value's digest is derived from its descendants, so no
value can contain itself), but a corrupt or hostile file can encode a cycle and
must be rejected rather than looped on.
"""


class SharedStateError(ValueError):
    """Raised for a malformed or unreadable shared-state document."""


def _float_repr(value: float) -> str:
    if value != value or value in (float("inf"), float("-inf")):
        raise SharedStateError(f"non-finite float is not representable in JSON: {value!r}")
    text = repr(float(value))
    if "." not in text and "e" not in text and "E" not in text:
        text += ".0"
    return text


def _utf8(key: Any) -> bytes:
    if not isinstance(key, str):
        raise SharedStateError(f"object key is not a string: {key!r}")
    return key.encode("utf-8")


def _digest(value: Any) -> bytes:
    """SHA-256 Merkle digest of a parsed JSON value (see SHARED_STATE_SPEC)."""
    if value is None:
        return hashlib.sha256(b"n").digest()
    if value is True:
        return hashlib.sha256(b"t").digest()
    if value is False:
        return hashlib.sha256(b"f").digest()
    if isinstance(value, int):
        return hashlib.sha256(b"i" + str(value).encode("ascii")).digest()
    if isinstance(value, float):
        return hashlib.sha256(b"d" + _float_repr(value).encode("ascii")).digest()
    if isinstance(value, str):
        return hashlib.sha256(b"s" + value.encode("utf-8")).digest()
    if isinstance(value, list):
        acc = hashlib.sha256(b"a")
        for item in value:
            acc.update(_digest(item).hex().encode("ascii"))
        return acc.digest()
    if isinstance(value, dict):
        acc = hashlib.sha256(b"o")
        for key in sorted(value, key=_utf8):
            acc.update(hashlib.sha256(b"s" + key.encode("utf-8")).digest().hex().encode("ascii"))
            acc.update(_digest(value[key]).hex().encode("ascii"))
        return acc.digest()
    raise SharedStateError(f"value of type {type(value).__name__} is not JSON")


def shared_state_digest(value: Any) -> str:
    """Full 64-character hex Merkle digest of a parsed JSON value."""
    return _digest(value).hex()


def _weight(value: Any) -> int:
    if value is None or value is True:
        return 4
    if value is False:
        return 5
    if isinstance(value, int):
        return len(str(value))
    if isinstance(value, float):
        return len(_float_repr(value))
    if isinstance(value, str):
        return 2 + len(value.encode("utf-8"))
    if isinstance(value, list):
        total = 2 + max(0, len(value) - 1)
        for item in value:
            total += _weight(item)
        return total
    if isinstance(value, dict):
        total = 2 + max(0, len(value) - 1)
        for key, item in value.items():
            total += 3 + len(key.encode("utf-8")) + _weight(item)
        return total
    raise SharedStateError(f"value of type {type(value).__name__} is not JSON")


class _Scan:
    """Pass 1: digest / occurrence-count / weight index over the encoded roots.

    Digests and weights are folded bottom-up in a single traversal (the naive
    "hash each node from scratch" shape is O(nodes * depth), which is far too
    slow on a 70 MB document).  ``by_id`` memoizes each live value's digest so
    pass 3 does not have to re-fold; the whole input document stays referenced
    for the duration of ``encode_shared_state``, so the ids are stable.
    """

    def __init__(self) -> None:
        self.counts: Dict[bytes, int] = {}
        self.nodes: Dict[bytes, Any] = {}
        self.weights: Dict[bytes, int] = {}
        self.by_id: Dict[int, bytes] = {}

    def visit(self, value: Any) -> Tuple[bytes, int]:
        if isinstance(value, list):
            acc = hashlib.sha256(b"a")
            weight = 2 + max(0, len(value) - 1)
            for item in value:
                child, child_weight = self.visit(item)
                acc.update(child.hex().encode("ascii"))
                weight += child_weight
            digest = acc.digest()
        elif isinstance(value, dict):
            acc = hashlib.sha256(b"o")
            weight = 2 + max(0, len(value) - 1)
            for key in sorted(value, key=_utf8):
                if not isinstance(key, str):
                    raise SharedStateError(f"object key is not a string: {key!r}")
                encoded_key = key.encode("utf-8")
                acc.update(hashlib.sha256(b"s" + encoded_key).digest().hex().encode("ascii"))
                child, child_weight = self.visit(value[key])
                acc.update(child.hex().encode("ascii"))
                weight += 3 + len(encoded_key) + child_weight
            digest = acc.digest()
        else:
            digest = _digest(value)
            weight = _weight(value)

        seen = self.counts.get(digest)
        if seen is None:
            self.counts[digest] = 1
            self.nodes[digest] = value
            self.weights[digest] = weight
        else:
            self.counts[digest] = seen + 1
        self.by_id[id(value)] = digest
        return digest, weight


def _select(scan: _Scan) -> Tuple[Dict[bytes, str], Dict[bytes, str]]:
    """Pass 2: choose interned strings and pooled containers; assign table keys."""
    interned: Dict[bytes, str] = {}
    pooled: Dict[bytes, str] = {}
    taken: Dict[str, bytes] = {}

    def claim(digest: bytes) -> str:
        key = digest.hex()[:SHARED_STATE_KEY_LEN]
        prior = taken.get(key)
        if prior is not None and prior != digest:
            raise SharedStateError(
                "shared-state table key collision on "
                f"{key!r}: {prior.hex()} vs {digest.hex()}"
            )
        taken[key] = digest
        return key

    for digest, count in scan.counts.items():
        if count < 2:
            continue
        node = scan.nodes[digest]
        weight = scan.weights[digest]
        inline_cost = count * weight
        if isinstance(node, str):
            entry_cost = _STRING_ENTRY_OVERHEAD + len(node.encode("utf-8"))
            if inline_cost > count * _REF_COST + entry_cost:
                interned[digest] = claim(digest)
        elif isinstance(node, (list, dict)):
            if inline_cost > count * _REF_COST + _POOL_ENTRY_OVERHEAD + weight:
                pooled[digest] = claim(digest)
    return interned, pooled


class _Emit:
    """Pass 3: build encoded nodes plus the `$strings` / `$pool` tables."""

    def __init__(self, scan: _Scan, interned: Dict[bytes, str], pooled: Dict[bytes, str]) -> None:
        self.by_id = scan.by_id
        self.interned = interned
        self.pooled = pooled
        self.strings: Dict[str, str] = {}
        self.pool: Dict[str, Any] = {}
        self._pool_done: set = set()

    def node(self, value: Any) -> Any:
        if isinstance(value, str):
            key = self.interned.get(self.by_id[id(value)])
            if key is None:
                return value
            self.strings[key] = value
            return {"$s": key}
        if isinstance(value, (list, dict)):
            digest = self.by_id[id(value)]
            key = self.pooled.get(digest)
            if key is None:
                return self.body(value)
            if key not in self._pool_done:
                # Reserve before recursing so a (corrupt) self-containing input
                # cannot recurse forever.
                self._pool_done.add(key)
                self.pool[key] = self.body(value)
            return {"$r": key}
        return value

    def body(self, value: Any) -> Any:
        if isinstance(value, list):
            return [self.node(item) for item in value]
        encoded = {}
        for key in sorted(value, key=_utf8):
            encoded[key] = self.node(value[key])
        if len(encoded) == 1 and next(iter(encoded)) in _SIGIL_KEYS:
            return {"$e": encoded}
        return encoded


def encode_shared_state(document: Any) -> Dict[str, Any]:
    """Encode a supervisor-state document into ``trellis-shared-state/1``.

    ``document`` must be the parsed outer object.  Returns a new object; the
    input is not mutated.  Values under keys other than
    ``SHARED_STATE_ENCODED_KEYS`` are deep-copied through unchanged.
    """
    if not isinstance(document, dict):
        raise SharedStateError(
            f"shared-state encoding needs a JSON object, got {type(document).__name__}"
        )
    for key in document:
        if not isinstance(key, str):
            raise SharedStateError(f"object key is not a string: {key!r}")
        if key.startswith("$"):
            raise SharedStateError(f"top-level key {key!r} conflicts with the shared-state envelope")

    scan = _Scan()
    for key in SHARED_STATE_ENCODED_KEYS:
        if key in document:
            scan.visit(document[key])
    interned, pooled = _select(scan)
    emit = _Emit(scan, interned, pooled)
    encoded_roots = {key: emit.node(document[key]) for key in SHARED_STATE_ENCODED_KEYS if key in document}

    out: Dict[str, Any] = {"$format": SHARED_STATE_FORMAT}
    for key, value in document.items():
        if key not in SHARED_STATE_ENCODED_KEYS:
            out[key] = copy.deepcopy(value)
    out["$strings"] = {key: emit.strings[key] for key in sorted(emit.strings)}
    out["$pool"] = {key: emit.pool[key] for key in sorted(emit.pool)}
    for key in SHARED_STATE_ENCODED_KEYS:
        if key in encoded_roots:
            out[key] = encoded_roots[key]
    return out


def is_shared_state(document: Any) -> bool:
    """Structural detection: no flag, no env var, no SHA-ancestry test."""
    return isinstance(document, dict) and "$format" in document


class _Expand:
    def __init__(self, strings: Dict[str, Any], pool: Dict[str, Any]) -> None:
        self.strings = strings
        self.pool = pool
        self._active: List[str] = []

    def node(self, value: Any) -> Any:
        if isinstance(value, list):
            return [self.node(item) for item in value]
        if not isinstance(value, dict):
            return value
        if len(value) == 1:
            key = next(iter(value))
            if key == "$r":
                return self.expand(value[key])
            if key == "$s":
                return self.string(value[key])
            if key == "$e":
                inner = value[key]
                if not isinstance(inner, dict):
                    raise SharedStateError(
                        f'"$e" must hold an object, got {type(inner).__name__}'
                    )
                return {k: self.node(v) for k, v in inner.items()}
        return {k: self.node(v) for k, v in value.items()}

    def string(self, key: Any) -> str:
        if not isinstance(key, str):
            raise SharedStateError(f'"$s" reference must be a string, got {type(key).__name__}')
        if key not in self.strings:
            raise SharedStateError(f'"$s" reference {key!r} is absent from $strings')
        text = self.strings[key]
        if not isinstance(text, str):
            raise SharedStateError(f"$strings[{key!r}] is not a string")
        return text

    def expand(self, key: Any) -> Any:
        if not isinstance(key, str):
            raise SharedStateError(f'"$r" reference must be a string, got {type(key).__name__}')
        if key not in self.pool:
            raise SharedStateError(f'"$r" reference {key!r} is absent from $pool')
        if key in self._active:
            cycle = " -> ".join(self._active + [key])
            raise SharedStateError(f"$pool reference cycle: {cycle}")
        self._active.append(key)
        try:
            return self.node(self.pool[key])
        finally:
            self._active.pop()


def decode_shared_state(document: Any) -> Any:
    """Return ``document`` expanded, or ``document`` itself if it is plain.

    Every ``$r`` occurrence expands to a fresh deep copy, so no two positions in
    the result share a mutable object.
    """
    if not is_shared_state(document):
        if isinstance(document, dict):
            for key in ("$strings", "$pool"):
                if key in document:
                    raise SharedStateError(
                        f"document carries {key} but no $format member; it is neither "
                        "a plain snapshot nor a well-formed shared-state document"
                    )
        return document
    fmt = document["$format"]
    if fmt != SHARED_STATE_FORMAT:
        raise SharedStateError(f"unsupported supervisor-state format: {fmt!r}")
    strings = document.get("$strings")
    pool = document.get("$pool")
    if not isinstance(strings, dict):
        raise SharedStateError("shared-state document has no $strings object")
    if not isinstance(pool, dict):
        raise SharedStateError("shared-state document has no $pool object")

    expand = _Expand(strings, pool)
    out: Dict[str, Any] = {}
    for key, value in document.items():
        if key in ("$format", "$strings", "$pool"):
            continue
        if isinstance(key, str) and key.startswith("$"):
            raise SharedStateError(
                f"unrecognised envelope member {key!r} for {SHARED_STATE_FORMAT}; "
                "an extension must bump $format"
            )
        if key in SHARED_STATE_ENCODED_KEYS:
            out[key] = expand.node(value)
        else:
            out[key] = copy.deepcopy(value)
    return out


def decode_shared_state_text(text: str) -> Any:
    """Parse ``text`` as JSON and decode it if it is a shared-state document."""
    return decode_shared_state(json.loads(text))


def load_supervisor_state(path: Path) -> Any:
    """Read and decode a supervisor-state file (old plain or new shared form)."""
    return decode_shared_state_text(Path(path).read_text(encoding="utf-8"))
