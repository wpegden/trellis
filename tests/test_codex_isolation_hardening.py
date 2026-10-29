"""Focused offline checks, runnable with unittest (no provider or pytest run)."""
import base64
from contextlib import ExitStack
import io
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from urllib.error import HTTPError

from trellis import codex_quota_api as api, quota_snapshots as quota
from trellis.adapters import ProviderConfig
from trellis.agents import codex_headless as headless


class QuotaIsolationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name)
        self.env = patch.dict(os.environ, {"CODEX_HOME": str(self.home)}, clear=True)
        self.env.start()
        self.addCleanup(self.env.stop)
        self.write_auth("access-before")
        (self.home / "config.toml").write_text("")
        self.payload = {"plan_type": "pro", "credits": {"balance": "42"}, "rate_limit": {
            "primary_window": {"used_percent": 12, "limit_window_seconds": 18000,
                               "reset_at": 2000000100},
            "secondary_window": {"used_percent": 25, "limit_window_seconds": 604800,
                                 "reset_after_seconds": 300},
        }, "additional_rate_limits": [{"rate_limit": {"used_percent": 99}}]}

    def write_auth(self, access):
        claims = base64.urlsafe_b64encode(b'{"email":"fixture@example.invalid"}').decode()
        auth = {"auth_mode": "chatgpt", "tokens": {
            "access_token": access, "refresh_token": "never-send-refresh",
            "account_id": "workspace-1", "id_token": f"e30.{claims}.fake"}}
        tmp = self.home / "next.json"
        tmp.write_text(json.dumps(auth))
        tmp.replace(self.home / "auth.json")

    def query(self, open_fn):
        with patch.object(api.urllib.request, "build_opener") as build, \
                patch.object(quota.subprocess, "run", side_effect=AssertionError("no CLI")), \
                patch.object(quota.time, "time", return_value=2000000000):
            build.return_value.open.side_effect = open_fn
            return quota.probe_codex(timeout_seconds=3)

    def test_read_only_correct_account_and_normalized_windows(self):
        before = {p.name: p.read_bytes() for p in self.home.iterdir()}

        def opened(req, timeout):
            self.assertEqual(req.full_url, "https://chatgpt.com/backend-api/wham/usage")
            self.assertEqual(req.get_header("Authorization"), "Bearer access-before")
            self.assertEqual(req.get_header("Chatgpt-account-id"), "workspace-1")
            self.assertNotIn("never-send-refresh", repr(req.__dict__))
            self.assertEqual(timeout, 3)
            return io.StringIO(json.dumps(self.payload))

        out = self.query(opened)
        self.assertTrue(out["ok"])
        self.assertEqual(out["account"], "fixture@example.invalid")
        self.assertEqual(out["plan_tier"], "pro")
        self.assertEqual(out["credits"], 42)
        self.assertEqual([w["name"] for w in out["windows"]], ["five_hour", "weekly"])
        self.assertEqual(out["windows"][1]["resets_at"], 2000000300)
        self.assertEqual(out["windows"][1]["monthly_burn_pct"], round(25 * 7 / 30, 4))
        self.assertEqual(before, {p.name: p.read_bytes() for p in self.home.iterdir()})

    def test_next_probe_reads_replaced_auth(self):
        seen = []

        def opened(req, timeout):
            seen.append(req.get_header("Authorization"))
            return io.StringIO(json.dumps(self.payload))

        self.assertTrue(self.query(opened)["ok"])
        self.write_auth("access-after")
        self.assertTrue(self.query(opened)["ok"])
        self.assertEqual(seen, ["Bearer access-before", "Bearer access-after"])

    def test_concurrent_probes_do_not_write_state(self):
        from concurrent.futures import ThreadPoolExecutor

        before = {p.name: p.read_bytes() for p in self.home.iterdir()}
        with patch.object(api.urllib.request, "build_opener") as build:
            build.return_value.open.side_effect = lambda *a, **kw: io.StringIO(json.dumps(self.payload))
            with ThreadPoolExecutor(max_workers=2) as pool:
                futures = [pool.submit(quota.probe_codex) for _ in range(2)]
                self.assertTrue(all(f.result()["ok"] for f in futures))
        self.assertEqual(before, {p.name: p.read_bytes() for p in self.home.iterdir()})

    def test_expired_auth_no_refresh_retry_or_secret_log(self):
        original = (self.home / "auth.json").read_bytes()
        calls = []

        def denied(req, timeout):
            calls.append(req)
            raise HTTPError(req.full_url, 401, "secret-value", {}, None)

        out = self.query(denied)
        self.assertFalse(out["ok"])
        self.assertEqual(len(calls), 1)
        self.assertNotIn("secret-value", json.dumps(out))
        self.assertEqual(original, (self.home / "auth.json").read_bytes())

    def test_unsupported_config_fails_before_network(self):
        configs = ['cli_auth_credentials_store = "keyring"',
                   'model_provider = "other"', 'forced_login_method = "api"',
                   'forced_chatgpt_workspace_id = "other"',
                   'chatgpt_base_url = "https://other.invalid"']
        for config in configs:
            with self.subTest(config=config):
                (self.home / "config.toml").write_text(config)
                with patch.object(api.urllib.request, "build_opener") as build:
                    self.assertFalse(quota.probe_codex()["ok"])
                    build.assert_not_called()

    def test_environment_auth_fails_before_network(self):
        with patch.dict(os.environ, {"CODEX_ACCESS_TOKEN": "other-token"}), \
                patch.object(api.urllib.request, "build_opener") as build:
            self.assertFalse(quota.probe_codex()["ok"])
            build.assert_not_called()

    def test_redirect_refused(self):
        self.assertIsNone(api._NoRedirect().redirect_request(
            None, None, 302, "redirect", {}, "https://other.invalid"))

    def test_unknown_or_malformed_response_fails(self):
        for payload in ({}, [], {"rate_limit": {"primary_window": {
                "used_percent": float("nan"), "limit_window_seconds": 18000}}}):
            with self.subTest(payload=payload):
                self.assertFalse(self.query(lambda *a, **kw: io.StringIO(json.dumps(payload)))["ok"])


class ExitGraceTests(unittest.TestCase):
    def run_burst(self, *, exit_at=None, dead_at=None, exit_code=0, event=True):
        from trellis import burst
        from trellis.agents import tmux_backend

        with tempfile.TemporaryDirectory() as temp, ExitStack() as stack:
            root = Path(temp)
            logs = root / "logs"
            done = root / "done"
            clock = [0.0]
            killed = []
            usage = {"input_tokens": 123, "output_tokens": 7}

            def sleep(seconds):
                clock[0] += seconds
                if exit_at is not None and clock[0] >= exit_at:
                    (logs / "worker.exit").write_text(str(exit_code))

            def tmux(*args, **kwargs):
                if args[0] == "display-message":
                    return SimpleNamespace(returncode=0, stdout=(
                        "1" if dead_at is not None and clock[0] >= dead_at else "0"))
                if args[0] == "new-window":
                    return SimpleNamespace(returncode=0, stdout="@9 %9\n", stderr="")
                if args[0] == "send-keys":
                    (logs / "worker.started").write_text("started")
                    done.touch()
                    (logs / "worker-output.log").write_text(
                        json.dumps({"type": "turn.completed", "usage": usage}) + "\n"
                        if event else "")
                if args[0] == "kill-session":
                    killed.append(clock[0])
                return SimpleNamespace(returncode=0, stdout="", stderr="")

            stack.enter_context(patch.object(burst, "tmux_cmd", side_effect=tmux))
            stack.enter_context(patch.object(burst, "tmux_ensure_session"))
            stack.enter_context(patch.object(burst, "tmux_pane_is_dead", side_effect=lambda _: (
                dead_at is not None and clock[0] >= dead_at)))
            stack.enter_context(patch.object(headless.time, "monotonic", side_effect=lambda: clock[0]))
            stack.enter_context(patch.object(headless.time, "sleep", side_effect=sleep))
            stack.enter_context(patch.object(tmux_backend, "_submit_probe_for_burst", return_value=None))
            stack.enter_context(patch.object(tmux_backend, "append_cost_ledger"))
            result = headless.run(ProviderConfig(provider="codex", model="gpt-5.5"),
                                  "fixture", role="worker", session_name="fixture",
                                  work_dir=root, log_dir=logs, done_file=done)
            return result, killed

    def test_natural_exit_after_completion_preserves_usage(self):
        result, killed = self.run_burst(exit_at=3.0)
        self.assertTrue(result.ok)
        self.assertEqual(result.usage["input_tokens"], 123)
        self.assertGreaterEqual(min(killed), 3.0)
        self.assertLess(min(killed), 3.2)

    def test_hung_after_completion_is_bounded(self):
        result, killed = self.run_burst()
        self.assertTrue(result.ok)
        self.assertEqual(min(killed), 4.5)
        self.assertEqual(result.usage["output_tokens"], 7)

    def test_late_nonzero_exit_is_preserved(self):
        result, _ = self.run_burst(exit_at=3.0, exit_code=7)
        self.assertFalse(result.ok)
        self.assertEqual(result.exit_code, 7)

    def test_dead_pane_stops_wait(self):
        _, killed = self.run_burst(dead_at=3.0)
        self.assertGreaterEqual(min(killed), 3.0)
        self.assertLess(min(killed), 3.2)

    def test_absent_completion_keeps_existing_usage_wait(self):
        with patch.object(headless, "DONE_FILE_TURN_COMPLETED_WAIT_SECONDS", 3):
            _, killed = self.run_burst(event=False)
        self.assertEqual(min(killed), 5.5)

    def test_unresponsive_tmux_cannot_overrun_grace(self):
        import subprocess

        with tempfile.TemporaryDirectory() as temp:
            def timeout_query(*args, **kwargs):
                self.assertGreater(kwargs["timeout"], 0)
                self.assertLessEqual(kwargs["timeout"], 2)
                raise subprocess.TimeoutExpired("tmux", kwargs["timeout"])

            headless._wait_for_process_exit(Path(temp) / "exit", "%1", timeout_query)


if __name__ == "__main__":
    unittest.main()
