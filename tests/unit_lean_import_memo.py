"""Standalone unittest: in-memory source/artifacts, no pytest or provers.

PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p unit_lean_import_memo.py -v
The differential oracle is the unchanged instrumentation commit 8a54643c.
"""
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack, contextmanager
import io
import json
from pathlib import Path
import random
import subprocess
import threading
import types
import unittest
from unittest.mock import patch

from trellis.atomic_actions import observations as obs
from trellis.checker import server


BASE = "8a54643c2c6bae8db4f657432a1816406de2b977"


def baseline_module():
    source = subprocess.check_output(
        ["git", "show", f"{BASE}:trellis/atomic_actions/observations.py"], text=True
    )
    module = types.ModuleType("baseline_observations")
    module.__file__ = obs.__file__
    exec(compile(source, module.__file__, "exec"), module.__dict__)
    return module


class MemoryFiles:
    """Only the read interface used by the predicates; never creates files."""

    def __init__(self, files):
        self.files = {str(path): data for path, data in files.items()}
        self.trace = []
        self.before_read = lambda path, operation: None

    def read(self, path, operation):
        path = str(path)
        self.trace.append((operation, path))
        self.before_read(path, operation)
        value = self.files.get(path, FileNotFoundError(path))
        if isinstance(value, Exception):
            raise value
        return value

    @contextmanager
    def installed(self):
        def read_bytes(path):
            return self.read(path, "read_bytes")

        def read_text(path, encoding=None, errors=None):
            data = self.read(path, "read_text")
            with io.TextIOWrapper(io.BytesIO(data), encoding=encoding, errors=errors) as f:
                return f.read()

        def exists(path):
            self.trace.append(("exists", str(path)))
            return str(path) in self.files and not isinstance(
                self.files[str(path)], FileNotFoundError)

        def stat(path, **kwargs):
            data = self.read(path, "stat")
            # All rewrites have the same metadata, including same-size edits.
            return types.SimpleNamespace(st_size=len(data), st_mtime_ns=1, st_ino=1)

        def open_file(path, mode="r", **kwargs):
            if mode != "rb":
                raise AssertionError(f"unexpected open: {path} {mode}")
            return io.BytesIO(self.read(path, "open:rb"))

        with ExitStack() as stack:
            for name, fn in [("read_bytes", read_bytes), ("read_text", read_text),
                             ("exists", exists), ("stat", stat), ("open", open_file)]:
                stack.enter_context(patch.object(Path, name, fn))
            yield self


def fixture(old):
    repo = Path("/virtual-import-memo-repo")
    files = {
        repo / ".trellis/scripts/check.py": b"# identity\n",
        repo / "lean-toolchain": b"leanprover/lean4:test\n",
        repo / "lakefile.lean": b"package test\n",
        repo / "Tablet/Preamble.lean": b"import Mathlib\n",
        repo / "Tablet/A.lean": b"import Tablet.B Tablet.C Tablet.Preamble\n",
        repo / "Tablet/B.lean": b"/- /- nested -/ -/ public import Tablet.D\n",
        repo / "Tablet/C.lean": b"import Tablet.D -- diamond\n",
        repo / "Tablet/D.lean": b"-- leaf\n",
    }
    fs = MemoryFiles(files)
    nodes = ["Preamble", "D", "B", "C", "A"]
    for node in nodes:
        fs.files[str(old._tablet_olean_path(repo, node))] = b"synthetic:" + node.encode()
    for suffix in (".server", ".private"):
        fs.files[str(old._tablet_olean_path(repo, "A")) + suffix] = suffix.encode()
    with fs.installed():
        for node in nodes:
            replay = old._expected_kernel_replay_attestation(repo, node)
            replay.update(
                declaration_manifest_version=old._DECLARATION_MANIFEST_VERSION,
                trusted_import_policy_version=old._TRUSTED_IMPORT_POLICY_VERSION,
                declaration_manifest=[{"name": node, "kind": "theorem"}],
                trusted_direct_imports=[],
                visibility_manifests=[dict(level=p["level"], declarations=[])
                                      for p in replay["artifact_bundle"]],
            )
            record = dict(schema_version=old._OLEAN_PROVENANCE_SCHEMA_VERSION,
                          source_closure_sha256=old.tablet_source_closure_hash(repo, node),
                          kernel_replay=replay)
            fs.files[str(old._olean_srcclosure_sidecar_path(repo, node))] = json.dumps(record).encode()
    fs.trace.clear()
    return repo, fs, nodes


def evidence(module, repo, nodes):
    with patch.object(server, "observations", module):
        ready = server.CheckerServer._closures_have_current_kernel_replay(
            types.SimpleNamespace(supervisor_repo=repo), ["A"])
    return json.dumps(dict(
        ready=ready, order=module.materialization_order(repo, ["A"]),
        nodes={node: dict(
            closure=module.tablet_source_closure_hash(repo, node),
            current=module.olean_is_content_current(repo, node),
            replay=module.olean_has_current_kernel_replay(repo, node),
            manifest=module.read_kernel_replay_declaration_manifest(repo, node),
            evidence=module.read_kernel_replay_artifact_evidence(repo, node),
        ) for node in nodes}), sort_keys=True, separators=(",", ":")).encode()


class ImportMemoTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old = baseline_module()

    def setUp(self):
        memo = patch.object(obs, "_lean_import_memo", obs._LeanImportMemo())
        memo.start()
        self.addCleanup(memo.stop)

    def compare(self, repo, fs, nodes):
        outputs, traces = [], []
        with fs.installed():
            for module in (self.old, obs, obs):
                fs.trace.clear()
                outputs.append(evidence(module, repo, nodes))
                traces.append(list(fs.trace))
        self.assertEqual(outputs[0], outputs[1])
        self.assertEqual(outputs[0], outputs[2])
        self.assertEqual(traces[0], traces[1])
        self.assertEqual(traces[0], traces[2])
        return json.loads(outputs[0])

    def test_identical_hashes_evidence_and_every_read_cold_and_warm(self):
        repo, fs, nodes = fixture(self.old)
        self.assertTrue(self.compare(repo, fs, nodes)["ready"])
        self.assertGreater(obs._lean_import_memo.info()["hits"], 0)

    def test_content_mutations_and_failures_cannot_hide_behind_memo(self):
        repo, initial, nodes = fixture(self.old)
        self.compare(repo, initial, nodes)  # retain old contents across cases
        source = str(repo / "Tablet/D.lean")
        olean = str(obs._tablet_olean_path(repo, "A"))
        sidecar = str(obs._olean_srcclosure_sidecar_path(repo, "A"))
        cases = [
            (source, b"-- edit\n"),  # identical size and stat metadata
            (source, b"import Tablet.Missing\n"),
            (source, FileNotFoundError()), (source, PermissionError()),
            (str(repo / ".trellis/scripts/check.py"), FileNotFoundError()),
            (str(repo / "Tablet/Preamble.lean"), FileNotFoundError()),
            (str(repo / "lean-toolchain"), b"changed pin"),
            (str(repo / "lake-manifest.json"), b"changed manifest"),
            (olean, b"corrupted:A"), (olean, b""), (olean, FileNotFoundError()),
            (olean + ".server", b"corrupted server"),
            (olean + ".private", b"corrupted private"),
            (sidecar, b"{}"), (sidecar, b"malformed"),
            (sidecar, FileNotFoundError()),
        ]
        for path, value in cases:
            with self.subTest(path=path, value=value):
                fs = MemoryFiles({**initial.files, path: value})
                # Permission errors in materialization_order already propagate;
                # compare that existing behavior separately below.
                if isinstance(value, PermissionError) and path == source:
                    with fs.installed():
                        for module in (self.old, obs):
                            self.assertIsNone(module.tablet_source_closure_hash(repo, "A"))
                            with self.assertRaises(PermissionError):
                                module.materialization_order(repo, ["A"])
                elif isinstance(value, FileNotFoundError) and path == source:
                    fs.files.pop(path)
                    self.assertFalse(self.compare(repo, fs, nodes)["ready"])
                else:
                    self.assertFalse(self.compare(repo, fs, nodes)["ready"])

    def test_replay_record_fields_and_legacy_sidecar_still_fail_closed(self):
        repo, initial, nodes = fixture(self.old)
        sidecar = str(obs._olean_srcclosure_sidecar_path(repo, "A"))
        record = json.loads(initial.files[sidecar])
        variants = [record["source_closure_sha256"].encode()]
        for field in record["kernel_replay"]:
            changed = json.loads(initial.files[sidecar])
            del changed["kernel_replay"][field]
            variants.append(json.dumps(changed).encode())
        for value in variants:
            with self.subTest(value=value):
                fs = MemoryFiles({**initial.files, sidecar: value})
                self.assertFalse(self.compare(repo, fs, nodes)["ready"])

    def test_mutation_between_reads_within_one_predicate(self):
        results, traces = [], []
        for module in (self.old, obs):
            repo, fs, _ = fixture(self.old)
            reads = 0

            def mutate(path, operation):
                nonlocal reads
                if path == str(repo / "Tablet/D.lean") and operation == "read_text":
                    reads += 1
                    if reads == 2:
                        fs.files[path] = b"import Tablet.NewDependency\n"

            fs.before_read = mutate
            with fs.installed(), patch.object(server, "observations", module):
                results.append(server.CheckerServer._closures_have_current_kernel_replay(
                    types.SimpleNamespace(supervisor_repo=repo), ["A"]))
            traces.append(fs.trace)
        self.assertEqual(results, [False, False])
        self.assertEqual(traces[0], traces[1])

    def test_cycles_missing_imports_and_raw_byte_hashes(self):
        repo, fs, nodes = fixture(self.old)
        for content in (b"import Tablet.A\n", b"import Tablet.Unknown\n", b"-- \xff\n", b"-- \xfe\n"):
            fs.files[str(repo / "Tablet/D.lean")] = content
            self.compare(repo, fs, nodes)
        with fs.installed():
            first = obs.tablet_source_closure_hash(repo, "A")
            fs.files[str(repo / "Tablet/D.lean")] = b"-- \xff\n"
            self.assertNotEqual(first, obs.tablet_source_closure_hash(repo, "A"))

    def test_existing_kernel_hash_pin(self):
        repo = Path("/virtual-pin")
        fs = MemoryFiles({
            repo / ".trellis/scripts/check.py": b"#!/usr/bin/env python3\n",
            repo / "lakefile.lean": "package «stub»\n".encode(),
            repo / "Tablet/Preamble.lean": b"import Mathlib.Data.Nat.Basic\n",
            repo / "Tablet/A.lean": b"import Tablet.Preamble\ntheorem A : True := trivial\n",
        })
        with fs.installed():
            for module in (self.old, obs, obs):
                self.assertEqual(module.tablet_source_closure_hash(repo, "A"),
                                 "a3967795e3d913306ab6974bdf36c943964b23e317a0cfa9546f6d80cf45f995")

    def test_parser_equivalence_and_mutable_return_isolation(self):
        rng = random.Random(17)
        pieces = ["import Tablet.A Tablet.Δ\n", "public meta import all Tablet.B\n",
                  "/- nested /- import Hidden -/ -/", "-- import Hidden\n",
                  "import\n Tablet.Multiline\n", "private import X.Y\n", "\r\n", "λ"]
        for _ in range(200):
            content = "".join(rng.choices(pieces, k=12))
            expected = self.old._lean_import_modules(content)
            for _ in range(2):
                actual = obs._lean_import_modules(content)
                self.assertEqual(actual, expected)
                actual.append("MUTATION")
                imports = obs._kernel_extract_tablet_imports(content)
                self.assertEqual(imports, self.old._kernel_extract_tablet_imports(content))
                imports.add("MUTATION")

    def test_lru_byte_entry_bounds_and_oversized_bypass(self):
        memo = obs._LeanImportMemo(max_bytes=4096, max_entries=2)
        memo.parse("import A")
        memo.parse("import B")
        memo.parse("import A")
        memo.parse("import C")
        self.assertEqual(memo.info()["evictions"], 1)
        self.assertEqual(list(memo._entries), ["import A", "import C"])
        for n in range(100):
            memo.parse(f"import N{n}\n" + "--" * 600)
            self.assertLessEqual(memo.info()["charged_bytes"], 4096)
            self.assertLessEqual(memo.info()["entries"], 2)
        before = dict(memo._entries)
        for content in ("--" * 4096, "import " + "A " * 200):
            self.assertEqual(memo.parse(content), self.old._lean_import_modules(content))
            self.assertEqual(dict(memo._entries), before)

    def test_concurrent_hits_misses_and_eviction(self):
        memo = obs._LeanImportMemo(max_bytes=4096, max_entries=8)
        contents = [f"import Tablet.N{n % 13}\n/- nested /- x -/ -/" for n in range(500)]
        with ThreadPoolExecutor(max_workers=8) as pool:
            results = list(pool.map(memo.parse, contents))
        self.assertEqual(results, [self.old._lean_import_modules(s) for s in contents])
        self.assertLessEqual(memo.info()["charged_bytes"], 4096)
        self.assertLessEqual(memo.info()["entries"], 8)

    def test_simultaneous_misses_install_one_entry(self):
        memo = obs._LeanImportMemo()
        barrier = threading.Barrier(8)
        original = obs._lean_import_modules_uncached

        def parse(content):
            barrier.wait(timeout=10)
            return original(content)

        with patch.object(obs, "_lean_import_modules_uncached", parse), \
             ThreadPoolExecutor(max_workers=8) as pool:
            values = list(pool.map(memo.parse, ["import Tablet.A"] * 8))
        self.assertEqual(values, [["Tablet.A"]] * 8)
        self.assertEqual(memo.info()["entries"], 1)
        self.assertEqual(memo.info()["misses"], 8)
        self.assertEqual(memo.info()["charged_bytes"],
                         next(iter(memo._entries.values()))[1])


if __name__ == "__main__":
    unittest.main()
