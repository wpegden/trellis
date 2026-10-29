"""Actual confined compilation through task-scoped authoritative RPC dispatch."""
from concurrent.futures import ThreadPoolExecutor
import json

from test_check_jobs_lean import make_repo, pytestmark
from trellis.checker import admission, workspaces
from trellis.checker.server import CheckerServer
from trellis.supervisor_workspace import sync_supervisor_workspace


def test_scoped_checks_do_not_reuse_another_tasks_sources_or_success(tmp_path, monkeypatch):
    monkeypatch.setenv("TRELLIS_LEAN_PARALLELISM", "1")
    monkeypatch.setattr(admission, "ACTIVE", admission.Admission(2))
    main = tmp_path / "runtime"
    runtimes = [main, main / "dispatch/1/runtime", main / "dispatch/2/runtime"]
    repos = []
    for runtime, proposition in zip(runtimes, ["False", "True", "False"]):
        repo = make_repo(runtime.parent)
        (repo / "Tablet/A.lean").write_text(
            f"-- [TABLET NODE: A]\ntheorem A : {proposition} :=\n-- BODY\nby trivial\n"
        )
        sync_supervisor_workspace(repo)
        runtime.mkdir(exist_ok=True)
        (runtime / "runtime_metadata.json").write_text(json.dumps({"repo_path": str(repo)}))
        repos.append(repo)
    server = CheckerServer(main)
    caps = [workspaces.register(main, runtime)["TRELLIS_CHECKER_TOKEN"] for runtime in runtimes[1:]]

    def check(cap):
        return json.loads(server._dispatch_line(json.dumps(dict(
            op="verify_node", request_id=1, node_name="A", auth_token=cap,
        )).encode()))

    try:
        with ThreadPoolExecutor(2) as pool:
            good, bad = list(pool.map(check, caps))
        assert good.get("returncode") == 0, json.dumps(good)
        assert bad.get("returncode", 0) != 0, json.dumps(bad)
        assert "A.lean" in bad.get("stdout", "") + bad.get("stderr", "")
        assert check(caps[0]).get("returncode") == 0
        source = repos[2] / "Tablet/A.lean"
        source.write_text(source.read_text().replace("False", "True"))
        repaired = check(caps[1])
        assert repaired.get("returncode") == 0, json.dumps(repaired)
        assert "False" in (repos[0] / "Tablet/A.lean").read_text()
    finally:
        server.shutdown()
