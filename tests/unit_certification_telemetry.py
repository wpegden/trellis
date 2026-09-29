"""Standalone unittest only: no pytest/conftest, prover, socket, or shared heap.

Run: python3 -m unittest discover -s tests -p unit_certification_telemetry.py -v
"""
import concurrent.futures
import ast
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import types
import unittest
from unittest.mock import patch

from trellis.atomic_actions import observations as obs
from trellis.checker import server, telemetry as tm
from trellis.checker import telemetry_resources as resources
from scripts.certification_denominators import summarize


class TelemetryTests(unittest.TestCase):
    def setUp(self):
        self.env = patch.dict(os.environ, {"TRELLIS_CHECKER_TELEMETRY": "1",
                                          "TRELLIS_CHECKER_TELEMETRY_BYTES": "0",
                                          "TRELLIS_CHECKER_TELEMETRY_RESOURCES": "0"})
        self.env.start()
        self.addCleanup(self.env.stop)

    def run_request(self, work):
        result = []

        @tm.deferred_log
        def emit(**kwargs):
            result.append(kwargs)

        @tm.request
        def run():
            try:
                return work()
            finally:
                emit(legacy="same")

        value = run()
        self.assertIsNone(tm._current.get())
        return value, result[0]

    def test_unicode_bytes_and_nested_cpu(self):
        text = "import Tablet.A -- λ\n/- outer /- inner -/ -/\ntheorem x := True"
        with patch.dict(os.environ, {"TRELLIS_CHECKER_TELEMETRY_BYTES": "1"}):
            value, row = self.run_request(lambda: obs._lean_import_modules(text))
        self.assertEqual(value, ["Tablet.A"])
        spans = row["measurement"]["spans"]
        strip = spans["strip_lean_comments"]
        self.assertEqual((strip["calls"], strip["chars"], strip["bytes"]),
                         (1, len(text), len(text.encode())))
        self.assertGreaterEqual(spans["lean_import_modules"]["wall_ms"], strip["wall_ms"])
        self.assertAlmostEqual(spans["lean_import_modules"]["self_wall_ms"] + strip["wall_ms"],
                               spans["lean_import_modules"]["wall_ms"])

    def test_counter_faults_do_not_change_value_or_application_exception(self):
        for hook in ("begin", "end", "snapshot"):
            with self.subTest(hook=hook), patch.object(tm.Measurement, hook, side_effect=RuntimeError("counter")):
                value, row = self.run_request(lambda: obs._strip_lean_comments("import A"))
                self.assertEqual(value, "import A")
                self.assertIn(row["measurement"]["status"], ("incomplete", "unavailable"))
        @tm.measured("raises")
        def fail():
            raise ValueError("certification failure")
        with patch.object(tm.Measurement, "end", side_effect=SystemExit("counter")):
            with self.assertRaisesRegex(ValueError, "certification failure"):
                self.run_request(fail)
        self.assertIsNone(tm._current.get())

    def test_log_failure_and_initialization_failure(self):
        @tm.deferred_log
        def bad_log(**kwargs):
            raise OSError("disk full")
        @tm.request
        def run():
            bad_log()
            return b"unchanged"
        self.assertEqual(run(), b"unchanged")
        with patch.object(tm, "Measurement", side_effect=MemoryError):
            self.assertEqual(run(), b"unchanged")

    def test_threads_and_reused_worker_are_isolated(self):
        barrier = threading.Barrier(4)
        def work(n):
            def parse():
                barrier.wait()
                for _ in range(n):
                    obs._strip_lean_comments("-- comment")
            return self.run_request(parse)[1]["measurement"]["spans"]["strip_lean_comments"]["calls"]
        with concurrent.futures.ThreadPoolExecutor(4) as pool:
            self.assertEqual(list(pool.map(work, range(1, 5))), [1, 2, 3, 4])
        _, row = self.run_request(lambda: None)
        self.assertEqual(row["measurement"]["spans"], {})

    def stub_server(self, directory, *, hit, warm=True, cls=None):
        cls = cls or server.CheckerServer
        obj = cls.__new__(cls)
        obj._check_request_token = lambda request: True
        obj._assert_path_containment = lambda request: None
        obj._sync_lock = threading.Lock()
        obj._workspace_lock = server._WorkspaceReaderWriterLock()
        obj.worker_repo = obj.supervisor_repo = obj.fingerprint_cache_path = Path(directory)
        obj._request_log = server._RequestLog(Path(directory) / "server.log")
        response = {"request_id": 7, "stdout": "evidence", "returncode": 0}
        def lookup(*args):
            # Deterministic omission regression: legacy duration excludes this.
            time.sleep(0.015)
            obs._strip_lean_comments("import Tablet.A -- λ")
            return (response, 0, {"local_closure_axioms_cache_hit": True}) if hit else None
        obj._try_cache_hit_only = tm.measured("cache_lookup")(lookup)
        obj._closures_have_current_kernel_replay = tm.measured("replay_readiness")(lambda nodes: warm)
        obj._dispatch_op = lambda *args: (response, 0, {"local_closure_axioms_cache_hit": False})
        return obj

    def test_real_dispatch_cache_hit_and_shared_exclusive_fallback(self):
        line = json.dumps(dict(op="local_closure_axioms", node_name="A", request_id=7)).encode()
        for hit, warm, modes in [(True, True, ["sync"]), (False, True, ["sync", "shared"]),
                                 (False, False, ["sync", "shared", "exclusive"])]:
            with self.subTest(hit=hit, warm=warm), tempfile.TemporaryDirectory() as directory:
                obj = self.stub_server(directory, hit=hit, warm=warm)
                with patch.object(server, "sync_tablet_dir", return_value={}):
                    wire = obj._dispatch_line(line)
                row = json.loads((Path(directory) / "server.log").read_text())
                self.assertEqual(json.loads(wire)["stdout"], "evidence")
                m = row["measurement"]
                self.assertEqual([lock["mode"] for lock in m["locks"]], modes)
                for lock in m["locks"]:
                    self.assertLessEqual(lock["waiting_ms"], lock["acquired_ms"])
                    self.assertLessEqual(lock["acquired_ms"], lock["released_ms"])
                self.assertEqual(m["subprocess_count"], 0)
                self.assertLessEqual(m["request_finished_ts"], row["ts"])
                if hit:
                    self.assertGreater(m["request_elapsed_ms"] - row["duration_ms"], 14)
                    self.assertEqual(row["lake_duration_ms"], 0)

    def test_failure_log_keeps_request_measurement(self):
        with tempfile.TemporaryDirectory() as directory:
            obj = self.stub_server(directory, hit=True)
            with patch.object(server, "sync_tablet_dir", side_effect=server.SyncError("sync failed")):
                wire = obj._dispatch_line(b'{"op":"print_axioms","node_name":"A","request_id":7}')
            row = json.loads((Path(directory) / "server.log").read_text())
            self.assertEqual(json.loads(wire)["rpc_error"]["kind"], "sync_failed")
            self.assertEqual(row["kind"], "sync_failed")
            self.assertIsNotNone(row["measurement"]["locks"][0]["released_ms"])

    def test_subprocess_success_timeout_and_resource_failure(self):
        # Replace ONLY command argv; real Python children run in a private /tmp
        # directory. There is no executable named lake on this path.
        original = obs._Wait4Popen
        for timeout in (False, True):
            def spawn(argv, **kwargs):
                script = "import time; time.sleep(0.3)" if timeout else "print('evidence')"
                return original([sys.executable, "-c", script], **kwargs)
            with tempfile.TemporaryDirectory() as directory, patch.object(obs, "_Wait4Popen", side_effect=spawn):
                value, row = self.run_request(lambda: obs._run_lake_command(
                    Path(directory), ["env"], timeout_secs=0.02 if timeout else 2))
            child = row["measurement"]["subprocesses"][0]
            self.assertGreater(child["elapsed_ms"], 0)
            self.assertIsNotNone(child["wait4_cpu_ms"])
            self.assertEqual(value["timed_out"], timeout)
        with patch.dict(os.environ, {"TRELLIS_CHECKER_TELEMETRY_RESOURCES": "1"}), \
             patch.object(resources, "Sampler", side_effect=RuntimeError("sampler")):
            proc = types.SimpleNamespace(pid=123, returncode=0, child_rusage=None)
            def work():
                ticket = tm.child_started(proc, "fake", time.monotonic_ns())
                tm.child_finished(ticket, proc)
                return "ok"
            self.assertEqual(self.run_request(work)[0], "ok")

    def test_lock_contention_is_wait_not_held(self):
        gate = threading.Lock()
        gate.acquire()
        timer = threading.Timer(0.03, gate.release)
        timer.start()
        def work():
            with tm.lock(gate, "exclusive"):
                pass
        _, row = self.run_request(work)
        timer.join()
        lock = row["measurement"]["locks"][0]
        self.assertGreater(lock["acquired_ms"] - lock["waiting_ms"], 15)
        self.assertLess(lock["released_ms"] - lock["acquired_ms"], 10)

    def test_resource_sampler_does_not_join(self):
        sampler = resources.Sampler(os.getpid())
        sampler.snapshot = {"cpu_sampled_ms": 1}
        self.assertEqual(sampler.stop(), sampler.snapshot)
        self.assertTrue(sampler.done.is_set())

    def test_resource_slots_and_child_history_are_bounded(self):
        held = []
        try:
            while resources._slots.acquire(blocking=False):
                held.append(True)
            self.assertIsNone(resources.start_sampler(os.getpid()))
        finally:
            for _ in held:
                resources._slots.release()
        proc = types.SimpleNamespace(pid=123, returncode=0, child_rusage=None)
        def work():
            for _ in range(260):
                tm.child_finished(tm.child_started(proc, "fake", time.monotonic_ns()), proc)
        _, row = self.run_request(work)
        m = row["measurement"]
        self.assertEqual((m["subprocess_count"], len(m["subprocesses"]),
                          m["subprocess_records_dropped"]), (260, 256, 4))
        self.assertGreater(m["subprocess_elapsed_ms"], 0)

    def test_spawn_failure_is_not_a_spawned_child(self):
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(obs, "_Wait4Popen", side_effect=FileNotFoundError("synthetic missing executable")):
            value, row = self.run_request(lambda: obs._run_lake_command(Path(directory), ["env"], timeout_secs=1))
        self.assertIn("synthetic missing executable", value["spawn_error"])
        self.assertEqual(row["measurement"]["subprocess_count"], 0)

    def test_freshness_stays_fail_closed_with_counter_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            (repo / "Tablet").mkdir()
            (repo / "Tablet/A.lean").write_text("theorem a : True := by trivial\n")
            (repo / "Tablet/Preamble.lean").write_text("-- synthetic\n")
            (repo / ".trellis/scripts").mkdir(parents=True)
            (repo / ".trellis/scripts/check.py").write_text("# synthetic\n")
            olean = obs._tablet_olean_path(repo, "A")
            olean.parent.mkdir(parents=True)
            olean.write_bytes(b"synthetic artifact; never executed")
            digest = obs.tablet_source_closure_hash(repo, "A")
            self.assertIsNotNone(digest)
            obs.write_olean_srcclosure(repo, "A", digest)
            sidecar = obs._olean_srcclosure_sidecar_path(repo, "A")
            before = sidecar.read_bytes()
            with patch.object(tm.Measurement, "begin", side_effect=RuntimeError("counter")):
                self.assertTrue(self.run_request(lambda: obs.olean_is_content_current(repo, "A"))[0])
                (repo / "Tablet/A.lean").write_text("changed source")
                self.assertFalse(self.run_request(lambda: obs.olean_is_content_current(repo, "A"))[0])
            self.assertEqual(sidecar.read_bytes(), before)

    def test_disabled_has_legacy_log_only(self):
        with patch.dict(os.environ, {"TRELLIS_CHECKER_TELEMETRY": "0"}):
            value, row = self.run_request(lambda: obs._strip_lean_comments("A"))
        self.assertEqual(value, "A")
        self.assertEqual(row, {"legacy": "same"})

    def test_denominators_include_two_acceptances_in_one_cycle(self):
        def record(index, event, commands=()):
            return dict(index=index, cycle=613, ts_ms=index * 1000,
                        event=event, commands=list(commands))
        issue = lambda rid: {"command": "issue_request", "request": {"id": rid, "kind": "Worker"}}
        worker = lambda rid: {"event": "wrapper_response", "response": {
            "kind": "worker", "request_id": rid, "status": "Ok", "outcome": "Valid"}}
        rows = [record(0, {"event": "start_cycle"}, [issue(1)]), record(1, worker(1)),
                record(2, {"event": "sidecar_closure"}, [{"command": "commit_checkpoint"}]),
                record(3, {"event": "other"}, [issue(2)]),
                record(4, worker(2), [{"command": "restore_worktree_to_active_worker_base"}]),
                record(5, worker(99))]
        row = summarize(rows)[0]
        self.assertEqual((row["cycle_starts"], row["accepted_source_transactions"],
                          row["worker_source_acceptances"], row["sidecar_acceptances"]), (1, 2, 1, 1))
        self.assertEqual(row["rejected_worker_responses"], 1)
        self.assertEqual(row["unclassified_worker_responses"], 1)
        with self.assertRaises(ValueError):
            summarize(rows + rows)

    def test_production_edits_are_only_instrumentation(self):
        class RemoveTelemetry(ast.NodeTransformer):
            def visit_ImportFrom(self, node):
                if node.module == "trellis.checker" and any(a.name == "telemetry" for a in node.names):
                    return None
                return node

            def visit_FunctionDef(self, node):
                node.decorator_list = [d for d in node.decorator_list if "telemetry." not in ast.unparse(d)]
                return self.generic_visit(node)

            def visit_Assign(self, node):
                if any(isinstance(t, ast.Name) and t.id == "telemetry_child" for t in node.targets):
                    return None
                return self.generic_visit(node)

            def visit_Expr(self, node):
                if ast.unparse(node).startswith("telemetry.child_finished("):
                    return None
                return self.generic_visit(node)

            def visit_Call(self, node):
                if ast.unparse(node.func) == "telemetry.lock":
                    return self.visit(node.args[0])
                return self.generic_visit(node)

        # The invariant: the instrumentation commit changed production code
        # only by adding telemetry — stripping telemetry from its tree yields
        # code AST-identical to its parent. Both sides are pinned to history
        # so later legitimate edits to these files cannot invalidate it.
        root = Path(__file__).resolve().parents[1]
        base = "296d33f3b2f3d0719bba0a23fda58237bf982416"
        instrumented = "8a54643c2c6bae8db4f657432a1816406de2b977"
        for path in ("trellis/atomic_actions/observations.py", "trellis/checker/server.py", "trellis/checker/sync.py"):
            original = subprocess.check_output(["git", "show", f"{base}:{path}"], cwd=root, text=True)
            candidate = subprocess.check_output(["git", "show", f"{instrumented}:{path}"], cwd=root, text=True)
            actual = RemoveTelemetry().visit(ast.parse(candidate))
            self.assertEqual(ast.dump(actual), ast.dump(ast.parse(original)), path)


if __name__ == "__main__":
    unittest.main()
