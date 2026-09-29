"""Contract tests for the ``trellis-shared-state/1`` supervisor-state codec.

The golden vectors under ``tests/fixtures/shared_state_vectors/`` are the
normative artifact: the JavaScript (``viewer/server.js``) and Rust
(``kernel/src``) implementations must reproduce them exactly.  The tests here
check the Python implementation against those vectors and pin the properties
the vectors cannot express on their own -- deep-copy on expansion, content-hash
table keys, encoder determinism, and fail-loud behaviour.
"""

from __future__ import annotations

import copy
import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from trellis.history_artifacts import (  # noqa: E402
    SHARED_STATE_FORMAT,
    SHARED_STATE_KEY_LEN,
    SharedStateError,
    decode_shared_state,
    decode_shared_state_text,
    encode_shared_state,
    is_shared_state,
    load_supervisor_state,
    shared_state_digest,
)

VECTOR_DIR = REPO_ROOT / "tests" / "fixtures" / "shared_state_vectors"


def _vectors(kind: str) -> list:
    out = []
    for path in sorted(VECTOR_DIR.glob("*.json")):
        vector = json.loads(path.read_text(encoding="utf-8"))
        if vector.get("kind") == kind:
            out.append(pytest.param(vector, id=path.stem))
    assert out, f"no {kind} vectors found in {VECTOR_DIR}"
    return out


def _canonical(value) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


# --------------------------------------------------------------- golden vectors


@pytest.mark.parametrize("vector", _vectors("roundtrip"))
def test_golden_roundtrip_vector(vector) -> None:
    plain = vector["plain"]
    encoded = encode_shared_state(plain)
    # Byte-for-byte, including member order: this is what the JS and Rust
    # implementations have to match.
    assert json.dumps(encoded, indent=2) == json.dumps(vector["encoded"], indent=2)
    decoded = decode_shared_state(vector["encoded"])
    assert json.dumps(decoded, indent=2) == json.dumps(vector["decoded"], indent=2)
    # Order-insensitive value equality with the original, with int/bool/float
    # distinctions preserved (plain ``==`` would let True pass for 1).
    assert shared_state_digest(decoded) == shared_state_digest(plain)


@pytest.mark.parametrize("vector", _vectors("failure"))
def test_golden_failure_vector(vector) -> None:
    with pytest.raises(SharedStateError) as excinfo:
        decode_shared_state(vector["document"])
    assert vector["error_contains"] in str(excinfo.value)


@pytest.mark.parametrize("vector", _vectors("digests"))
def test_golden_merkle_digests(vector) -> None:
    for entry in vector["digests"]:
        assert shared_state_digest(entry["value"]) == entry["digest"], entry["label"]


def test_merkle_digest_separates_types() -> None:
    # A JSON number's int/float split, and container tags, must all be distinct.
    distinct = [0, 0.0, False, None, "0", [], {}, [0], {"0": 0}]
    digests = [shared_state_digest(value) for value in distinct]
    assert len(set(digests)) == len(distinct)


# -------------------------------------------------------------- old-format read


def test_plain_document_is_returned_unchanged() -> None:
    plain = {"event_count": 7, "metadata": {}, "state": {"a": [1, 2]}, "commands": []}
    assert decode_shared_state(plain) is plain


@pytest.mark.parametrize(
    "value",
    [{}, {"state": {}}, [], "text", 3, None, {"format": SHARED_STATE_FORMAT}],
)
def test_detection_is_structural(value) -> None:
    assert not is_shared_state(value)
    assert decode_shared_state(value) is value


def test_detection_needs_no_flag_or_ancestry() -> None:
    encoded = encode_shared_state({"state": {"a": 1}})
    assert is_shared_state(encoded)
    assert decode_shared_state(encoded) == {"state": {"a": 1}}


def test_load_supervisor_state_reads_both_forms(tmp_path: Path) -> None:
    plain = {"event_count": 2, "state": {"a": [1, 1, 1]}}
    old = tmp_path / "old.json"
    old.write_text(json.dumps(plain, indent=2) + "\n", encoding="utf-8")
    new = tmp_path / "new.json"
    new.write_text(json.dumps(encode_shared_state(plain), indent=2) + "\n", encoding="utf-8")
    assert load_supervisor_state(old) == plain
    assert load_supervisor_state(new) == plain


# ------------------------------------------------------------------- fail loud


def test_truncated_file_raises(tmp_path: Path) -> None:
    doc = encode_shared_state({"state": {"a": "x" * 200, "b": "x" * 200, "c": "x" * 200}})
    text = json.dumps(doc, indent=2)
    path = tmp_path / "truncated.json"
    path.write_text(text[: len(text) // 2], encoding="utf-8")
    with pytest.raises(ValueError):
        load_supervisor_state(path)


def test_unknown_format_is_not_a_passthrough() -> None:
    with pytest.raises(SharedStateError):
        decode_shared_state({"$format": "something-else", "$strings": {}, "$pool": {}})


def test_missing_string_table_raises() -> None:
    with pytest.raises(SharedStateError):
        decode_shared_state({"$format": SHARED_STATE_FORMAT, "$pool": {}, "state": {}})


def test_encoder_rejects_reserved_top_level_keys() -> None:
    with pytest.raises(SharedStateError):
        encode_shared_state({"$pool": {}, "state": {}})


def test_encoder_rejects_non_json_values() -> None:
    with pytest.raises(SharedStateError):
        encode_shared_state({"state": {"when": object()}})


def test_encoder_rejects_non_finite_floats() -> None:
    with pytest.raises(SharedStateError):
        encode_shared_state({"state": {"x": float("inf")}})


def test_decode_requires_an_object_document() -> None:
    # A bare list has no "$format" member, so it is plain by definition.
    assert decode_shared_state([1, 2, 3]) == [1, 2, 3]


# ------------------------------------------------------- aliasing / deep copies


def test_expansion_deep_copies_every_alias() -> None:
    record = {"deps": ["Alpha"], "hashes": {"Alpha": "a" * 64}, "status": "Closed"}
    plain = {
        "state": {
            "local_closure_records": {"Alpha": record},
            "committed_local_closure_records": {"Alpha": record},
            "last_clean_local_closure_records": {"Alpha": record},
        }
    }
    decoded = decode_shared_state(encode_shared_state(plain))
    state = decoded["state"]
    a = state["local_closure_records"]
    b = state["committed_local_closure_records"]
    c = state["last_clean_local_closure_records"]
    assert a is not b and b is not c and a is not c
    assert a["Alpha"] is not b["Alpha"]
    assert a["Alpha"]["deps"] is not b["Alpha"]["deps"]
    assert a["Alpha"]["hashes"] is not b["Alpha"]["hashes"]
    # scripts/backfill_sound_fingerprints_history.py mutates a parsed wrapper in
    # place and rewrites git history from it; a shared alias would corrupt the
    # other copies.
    a["Alpha"]["hashes"]["Alpha"] = "mutated"
    a["Alpha"]["deps"].append("Beta")
    assert b["Alpha"]["hashes"]["Alpha"] == "a" * 64
    assert c["Alpha"]["deps"] == ["Alpha"]


def test_encode_does_not_mutate_or_capture_the_input() -> None:
    plain = {"event_count": 1, "metadata": {"a": [1]}, "state": {"x": {"y": 1}}}
    before = copy.deepcopy(plain)
    encoded = encode_shared_state(plain)
    assert plain == before
    encoded["metadata"]["a"].append(2)
    assert plain["metadata"]["a"] == [1]


# -------------------------------------------------- content-hash table keying


def _sharing_document(mark: str) -> dict:
    """A document with the shape that matters.

    Three near-identical closure-record maps, whose entries are themselves
    drawn from a much smaller set of distinct record values -- the real file
    holds 13,433 ``kernel_semantic_hashes`` entries drawn from 1,008 distinct
    key/value pairs.
    """
    hashes = [f"{i:064x}" for i in range(20)]
    records = {
        f"Node{i:03d}": {
            "deps": [f"Dep{j:03d}" for j in range(i % 5)],
            "kernel_semantic_hashes": {"own": hashes[i % 20], "dep": hashes[(i * 7) % 20]},
            "status": "Closed",
        }
        for i in range(200)
    }
    records["Node017"] = dict(records["Node017"], status=mark)
    return {
        "event_count": 900,
        "checkpoint": {"cycle": 42, "committed": {"present_nodes": sorted(records)}},
        "state": {
            "cycle": 42,
            "committed": {"present_nodes": sorted(records)},
            "local_closure_records": records,
            "committed_local_closure_records": records,
            "last_clean_local_closure_records": records,
        },
    }


def test_pool_and_string_keys_are_content_hashes() -> None:
    """The anti-index-keying invariant.

    Every table key must be the leading hex of the Merkle digest of what it
    stores.  A sequential-index interner cannot satisfy this, which is what
    makes the check a regression test for it.
    """
    encoded = encode_shared_state(_sharing_document("Closed"))
    assert encoded["$pool"] and encoded["$strings"]
    for key, body in encoded["$pool"].items():
        expanded = decode_shared_state(
            {
                "$format": SHARED_STATE_FORMAT,
                "$strings": encoded["$strings"],
                "$pool": encoded["$pool"],
                "state": body,
            }
        )["state"]
        assert shared_state_digest(expanded)[:SHARED_STATE_KEY_LEN] == key
    for key, text in encoded["$strings"].items():
        assert shared_state_digest(text)[:SHARED_STATE_KEY_LEN] == key


def test_tables_are_emitted_sorted() -> None:
    encoded = encode_shared_state(_sharing_document("Closed"))
    for table in ("$strings", "$pool"):
        keys = list(encoded[table])
        assert keys == sorted(keys)


def test_table_keys_are_stable_across_a_small_edit() -> None:
    """Git delta locality: a one-record edit must move only that record's keys.

    Measured on two consecutive real checkpoints of the live run: the encoded
    pair packs to 1.99 MB against 7.57 MB for the plain pair, and over ten
    consecutive checkpoints the encoded pack grows ~4.4 kB per checkpoint
    against ~202 kB per checkpoint for a sequential-index encoding of the same
    data.  This test is the cheap, hermetic stand-in for that measurement.
    """
    before = encode_shared_state(_sharing_document("Closed"))
    after = encode_shared_state(_sharing_document("Open"))
    for table in ("$strings", "$pool"):
        keys_before = set(before[table])
        keys_after = set(after[table])
        shared = keys_before & keys_after
        assert len(shared) >= 0.9 * len(keys_before), table
        assert all(before[table][k] == after[table][k] for k in shared), table


def test_encoding_is_deterministic() -> None:
    plain = _sharing_document("Closed")
    first = json.dumps(encode_shared_state(plain), indent=2)
    second = json.dumps(encode_shared_state(json.loads(json.dumps(plain))), indent=2)
    assert first == second


def test_encoder_output_is_acyclic() -> None:
    """A `$r` cycle cannot arise from the encoder.

    A value's table key is derived from its descendants, so a value can never
    contain itself; the pool's reference graph is therefore a DAG.  Verified
    here by topological reduction over a heavily shared document.
    """
    encoded = encode_shared_state(_sharing_document("Closed"))

    def refs(node) -> set:
        if isinstance(node, list):
            out = set()
            for item in node:
                out |= refs(item)
            return out
        if isinstance(node, dict):
            if len(node) == 1 and next(iter(node)) == "$r":
                return {node["$r"]}
            out = set()
            for item in node.values():
                out |= refs(item)
            return out
        return set()

    graph = {key: refs(body) for key, body in encoded["$pool"].items()}
    assert graph
    remaining = dict(graph)
    while remaining:
        leaves = [k for k, deps in remaining.items() if not (deps & remaining.keys())]
        assert leaves, f"cycle among {sorted(remaining)}"
        for leaf in leaves:
            remaining.pop(leaf)


def test_compression_on_a_representative_document() -> None:
    """The fixture compresses 8.5x; the live 70 MB checkpoint compresses 7.1x
    at ``indent=2`` and 9.7x compact.  The bound is loose on purpose -- this
    guards against a selection rule that stops sharing, not against drift in
    the ratio."""
    plain = _sharing_document("Closed")
    plain_bytes = len(json.dumps(plain, indent=2).encode())
    encoded_bytes = len(json.dumps(encode_shared_state(plain), indent=2).encode())
    assert encoded_bytes * 5 < plain_bytes


# ------------------------------------------------------- real-history corpus


_HISTORY_REPOS = os.environ.get("TRELLIS_SHARED_STATE_HISTORY_REPOS", "")


@pytest.mark.skipif(
    not _HISTORY_REPOS,
    reason="set TRELLIS_SHARED_STATE_HISTORY_REPOS=<repo>[:<repo>...] to replay real checkpoints",
)
def test_roundtrip_over_real_checkpoint_history() -> None:
    path = ".trellis-history/supervisor_state.json"
    env = dict(os.environ, GIT_OPTIONAL_LOCKS="0")
    checked = 0
    for repo in _HISTORY_REPOS.split(":"):
        shas = subprocess.run(
            ["git", "-C", repo, "log", "--format=%H", "--all", "--", path],
            capture_output=True, text=True, check=True, env=env,
        ).stdout.split()
        shas.reverse()
        step = max(1, len(shas) // 20)
        for sha in shas[::step]:
            raw = subprocess.run(
                ["git", "-C", repo, "show", f"{sha}:{path}"],
                capture_output=True, check=True, env=env,
            ).stdout
            doc = json.loads(raw.decode("utf-8"))
            assert decode_shared_state(doc) is doc, f"{repo} {sha} identity"
            text = json.dumps(encode_shared_state(doc), separators=(",", ":"))
            back = decode_shared_state_text(text)
            assert shared_state_digest(back) == shared_state_digest(doc), f"{repo} {sha}"
            checked += 1
    assert checked


@pytest.mark.skipif(
    not _HISTORY_REPOS,
    reason="set TRELLIS_SHARED_STATE_HISTORY_REPOS=<repo>[:<repo>...] to pack real checkpoints",
)
def test_delta_pack_over_consecutive_real_checkpoints(tmp_path: Path) -> None:
    """Two consecutive real checkpoints, plain against encoded, packed by git.

    Measured on the live run at cycle 1086: plain 7,570,280 B, encoded
    1,987,814 B.  The bound below is deliberately slack; the point is that
    content-hash table keys keep the encoded pack a small fraction of the
    plain one rather than approaching it.
    """
    repo = _HISTORY_REPOS.split(":")[0]
    path = ".trellis-history/supervisor_state.json"
    env = dict(os.environ, GIT_OPTIONAL_LOCKS="0")
    shas = subprocess.run(
        ["git", "-C", repo, "log", "--format=%H", "-2", "--", path],
        capture_output=True, text=True, check=True, env=env,
    ).stdout.split()
    assert len(shas) == 2
    shas.reverse()
    blobs = [
        subprocess.run(
            ["git", "-C", repo, "show", f"{sha}:{path}"],
            capture_output=True, check=True, env=env,
        ).stdout
        for sha in shas
    ]
    sizes = {}
    for name, payloads in (
        ("plain", blobs),
        (
            "encoded",
            [
                (json.dumps(encode_shared_state(json.loads(b.decode())), indent=2) + "\n").encode()
                for b in blobs
            ],
        ),
    ):
        work = tmp_path / name
        work.mkdir()
        subprocess.run(["git", "-C", str(work), "init", "-q"], check=True, env=env)
        subprocess.run(["git", "-C", str(work), "config", "user.email", "t@t"], check=True, env=env)
        subprocess.run(["git", "-C", str(work), "config", "user.name", "t"], check=True, env=env)
        for i, payload in enumerate(payloads):
            (work / "s.json").write_bytes(payload)
            subprocess.run(["git", "-C", str(work), "add", "s.json"], check=True, env=env)
            subprocess.run(["git", "-C", str(work), "commit", "-q", "-m", f"c{i}"], check=True, env=env)
        subprocess.run(
            ["git", "-C", str(work), "repack", "-adq", "--window=250", "--depth=250"],
            check=True, env=env,
        )
        pack = work / ".git" / "objects" / "pack"
        sizes[name] = sum(p.stat().st_size for p in pack.iterdir())
    assert sizes["encoded"] * 2 < sizes["plain"], sizes


def test_canonical_comparison_helper_is_order_insensitive() -> None:
    assert _canonical({"a": 1, "b": 2}) == _canonical({"b": 2, "a": 1})
