"""Meaning and transport regressions for Lean-native statement transformations."""
import json
import os
from pathlib import Path
import shutil
import subprocess

import pytest

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "kernel/tests/fixtures/local_closure_smoke"
SCRIPT = ROOT / "scripts/lean_statement_transform.lean"
SEMANTIC_SCRIPT = ROOT / "scripts/lean_semantic_fingerprint.lean"


def run_lean(*args):
    explicit = os.environ.get("TRELLIS_FIXTURE_LEAN")
    env = dict(os.environ)
    if explicit:
        env["LEAN_SYSROOT"] = str(Path(explicit).resolve().parent.parent)
        command = [explicit]
    else:
        assert shutil.which("lake"), "pinned Lean fixture is required"
        command = ["lake", "env", "lean"]
    return subprocess.run([*command, *map(str, args)], cwd=FIXTURE, env=env,
                          text=True, capture_output=True, timeout=45)


def derive(statement, condition=None, arguments=None, context="import Lean\n"):
    inp = dict(name="Original", statement=statement, context=context)
    if condition is not None:
        inp["condition"] = condition
    if arguments is not None:
        inp["arguments"] = arguments
    process = run_lean("--run", SCRIPT, json.dumps(inp))
    lines = [line for line in process.stdout.splitlines() if line.startswith("{")]
    assert len(lines) == 1, (process.stdout, process.stderr)
    return process, json.loads(lines[0])


@pytest.mark.parametrize("statement,condition,arguments,expected", [
    ("theorem Original : (∀ n : Nat, n = n) := by", "n ≠ 1", ["Nat.succ 0"],
     ("¬ (∀ n : Nat, n = n)", "∀ n : Nat, n ≠ 1 → n = n", "∃ n : Nat, n ≠ 1", "¬ (Nat.succ 0 ≠ 1)")),
    ("theorem Original : forall n m : Nat, n = m := by", "n < m", ["0 + 1", "Nat.succ 1"],
     ("¬ (∀ n m : Nat, n = m)", "∀ n m : Nat, n < m → n = m", "∃ n m : Nat, n < m", "¬ (0 + 1 < Nat.succ 1)")),
    ("theorem Original (n : Nat) : ∀ (v w : Fin (n + 1)), v = w := by", "n < 3", None,
     ("¬ (∀ (n : Nat) (v w : Fin (n + 1)), v = w)", "∀ (n : Nat) (v w : Fin (n + 1)), n < 3 → v = w", "∃ (n : Nat) (v w : Fin (n + 1)), n < 3", None)),
    ("theorem Original : ∀ n : Nat, n = 0 → n = 1 := by", "n < 2", ["0"],
     ("¬ (∀ n : Nat, n = 0 → n = 1)", "∀ n : Nat, n < 2 → (n = 0 → n = 1)", "∃ n : Nat, n < 2", "¬ ((0 : Nat) < 2)")),
    ("theorem Original (f : Nat → Nat) : ∀ n : Nat, f n = n := by", "f n < 3", ["fun n => n + 1", "Nat.succ 0"],
     ("¬ (∀ (f : Nat → Nat) (n : Nat), f n = n)", "∀ (f : Nat → Nat) (n : Nat), f n < 3 → f n = n", "∃ (f : Nat → Nat) (n : Nat), f n < 3", "¬ (Nat.succ 0 + 1 < 3)")),
    ("theorem Original {n : Nat} : n = n := by", "n < 3", ["1 + 2"],
     ("¬ (∀ {n : Nat}, n = n)", "∀ {n : Nat}, n < 3 → n = n", "∃ n : Nat, n < 3", "¬ ((1 + 2 : Nat) < 3)")),
    ("theorem Original [inst : Inhabited Nat] : (default : Nat) = 0 := by", "(default : Nat) < 3", ["⟨Nat.succ 0⟩"],
     ("¬ (∀ [inst : Inhabited Nat], (default : Nat) = 0)", "∀ [inst : Inhabited Nat], (default : Nat) < 3 → (default : Nat) = 0", "∃ inst : Inhabited Nat, @Inhabited.default Nat inst < 3", "¬ (Nat.succ 0 < 3)")),
    ("theorem Original (n : Nat) : n = (let x := n; x) := by", 'let text : String := "[}]"; n < text.length /- } -/', ["let x := 0; x + 1"],
     ("¬ (∀ n : Nat, n = n)", '∀ n : Nat, (let text : String := "[}]"; n < text.length) → n = n', '∃ n : Nat, n < "[}]".length', '¬ ((1 : Nat) < "[}]".length)')),
])
def test_generated_types_have_exact_meaning(tmp_path, statement, condition, arguments, expected):
    process, row = derive(statement, condition, arguments)
    assert process.returncode == 0 and row["status"] == "ok", row
    out = row["result"]
    generated = [out["refutation_type"], out["conditional"]["proof_type"], out["conditional"]["inhabited_type"], out["conditional"].get("excluded_type")]
    source = "import Lean\n"
    for actual, wanted in zip(generated, expected):
        assert (actual is None) == (wanted is None)
        if actual is not None:
            source += f"example : ({actual}) = ({wanted}) := rfl\n"
    file = tmp_path / "Meaning.lean"
    file.write_text(source)
    check = run_lean(file)
    assert check.returncode == 0, check.stdout + check.stderr


def test_scoped_notation_namespace_and_options(tmp_path):
    context = "import Lean\nnamespace Scoped\nscoped notation \"small\" n => n < (3 : Nat)\nend Scoped\nopen scoped Scoped\nnamespace Test\nset_option autoImplicit false\n"
    process, row = derive("theorem Original (n : Nat) : small n := by", "small (n + 1)", ["Nat.succ 0"], context)
    assert process.returncode == 0, row
    file = tmp_path / "Scoped.lean"
    file.write_text(context + f'example : ({row["result"]["conditional"]["proof_type"]}) = (∀ n : Nat, n + 1 < 3 → n < 3) := rfl\nend Test\n')
    result = run_lean(file)
    assert result.returncode == 0, result.stdout + result.stderr


@pytest.mark.parametrize("statement,condition,arguments", [
    ("theorem Original : ∀ n : Nat, n = n := by", "n < 3", []),
    ("theorem Original : ∀ n : Nat, n = n := by", "n < 3", ["True"]),
    ("theorem Original (n : Nat) : n = n := by", "(n < 3", None),
    ("theorem Original (n : Nat) : n = n := by", "True", None),
    ("theorem Original (n : Nat) : n = n := by", "n = n", None),
    ("theorem Original (n : Nat) : n = n := by", "n < 3", ["(by sorry : Nat)"]),
    ("theorem Other : True := by", None, None),
    ("theorem Original : ∀ n, n + 1 = n := by", "List.length n < 3", None),
    ("theorem Original {n : Nat} : n = n := by", None, None),
    ("theorem Original.{u} (n : Nat) : n = n := by", None, None),
    ("theorem Original : UnknownType := by", None, None),
])
def test_invalid_input_fails_preflight(statement, condition, arguments):
    process, row = derive(statement, condition, arguments)
    assert process.returncode != 0 and row["status"] == "invalid_statement", row
    assert "result" not in row


def test_refutation_does_not_require_or_install_target_proof():
    process, row = derive("theorem Original (n : Nat) : n < 0 := by")
    assert process.returncode == 0, row
    assert "Original" not in row["result"]["refutation_type"]


def test_match_types_are_reusable_in_a_fresh_lean_environment(tmp_path):
    # Each match creates a temporary matcher during elaboration. Diagnostic
    # pp.all output exposes its internal name rather than reusable Lean source.
    proposition = "∀ xs : List Nat, match xs with | [] => True | n :: _ => n = 0"
    condition = "match xs with | [] => False | n :: _ => n < 3"
    witness = "match (some 1 : Option Nat) with | none => [] | some n => [n]"
    process, row = derive(f"theorem Original : {proposition} := by", condition, [witness])
    assert process.returncode == 0 and row["status"] == "ok", row
    out = row["result"]
    types = [out["refutation_type"], out["conditional"]["proof_type"],
             out["conditional"]["inhabited_type"], out["conditional"]["excluded_type"]]
    expected = [f"¬ ({proposition})",
                f"∀ xs : List Nat, ({condition}) → (match xs with | [] => True | n :: _ => n = 0)",
                f"∃ xs : List Nat, {condition}", "¬ ((1 : Nat) < 3)"]
    path = tmp_path / "MatchMeaning.lean"
    # Compare the unfolded recursors with surface matches using full kernel
    # reduction rather than the elaborator's blocked-matcher heuristic.
    path.write_text("import Lean\nset_option smartUnfolding false\n" + "\n".join(
        f"example : ({actual}) = ({wanted}) := rfl" for actual, wanted in zip(types, expected)))
    result = run_lean(path)
    assert result.returncode == 0, result.stdout + result.stderr


def test_checker_protocol_rejects_malformed_derivations():
    from trellis.checker.protocol import validate_request, ProtocolError
    envelope = dict(op="lean_statement_transform", request_id=1)
    for inp in [{}, {"name":"Original", "statement":"x", "context":"", "arguments":[]},
                {"name":"Original", "statement":"x", "context":"", "output":"forged"},
                {"name":"Original", "statement":"x", "context":"", "fingerprint_node":"../Outside"},
                {"name":"Original", "statement":"x", "context":"", "fingerprint_node":"Original", "condition":"True"}]:
        with pytest.raises(ProtocolError):
            validate_request({**envelope, "input":inp})
    validate_request({**envelope, "input":dict(name="Original", statement="x", context="")})
    validate_request({**envelope, "input":dict(name="Original", statement="x", context="", fingerprint_node="Original")})


def test_statement_transform_uses_standard_lean_replay_budget(monkeypatch, capsys):
    """A cold imported closure must outlive the former two-minute RPC cap."""
    from trellis.atomic_actions import checker_client, cli, observations

    seen = []
    monkeypatch.setattr(checker_client, "_validate_socket_path", lambda path: Path(path))
    monkeypatch.setattr(checker_client, "_round_trip", lambda socket, request, *, timeout_secs:
                        seen.append((request["timeout_secs"], timeout_secs)) or {"request_id":request["request_id"], "status":"ok"})
    inp = {"name":"Original", "statement":"theorem Original : True := by", "context":"import Lean\n"}
    checker_client.client_statement_transform(Path("/tmp/unused-checker.sock"), inp)
    assert seen == [(observations.LEAN_SUPPORT_TIMEOUT_SECS, observations.LEAN_SUPPORT_TIMEOUT_SECS)]

    monkeypatch.setattr(cli, "_resolve_socket_path", lambda: Path("/tmp/unused-checker.sock"))
    monkeypatch.setattr(cli, "client_statement_transform", lambda socket, input, *, timeout_secs:
                        seen.append((timeout_secs, timeout_secs)) or {"request_id":1, "status":"ok"})
    assert cli._main(["lean-statement-transform", "--input-json", json.dumps(inp)]) == 0
    capsys.readouterr()
    assert seen[-1] == (observations.LEAN_SUPPORT_TIMEOUT_SECS, observations.LEAN_SUPPORT_TIMEOUT_SECS)


def test_real_confined_checker_round_trip_and_cache():
    """Exercise socket transport, bwrap, pinned Lean and the existing cache."""
    import tempfile
    import threading
    from trellis.checker.server import CheckerServer
    from trellis.atomic_actions.checker_client import client_statement_transform

    scratch = ROOT / "kernel/.tmp-tests"
    scratch.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="lst-", dir="/tmp") as tmp, tempfile.TemporaryDirectory(dir=scratch) as project:
        repo = Path(project)
        (repo / ".trellis").mkdir()
        runtime = Path(tmp)
        (runtime / "runtime_metadata.json").write_text(json.dumps({"repo_path":str(repo)}))
        (repo / "Tablet").mkdir()
        server = CheckerServer(runtime, parallelism=1, socket_group_gid=None)
        server.supervisor_repo.mkdir(parents=True, exist_ok=True)
        for target in (repo, server.supervisor_repo):
            (target / "lean-toolchain").write_text("leanprover/lean4:v4.33.0\n")
            (target / "lakefile.lean").write_text('import Lake\nopen Lake DSL\npackage transformFixture\nlean_lib Tablet\n')
            (target / "lake-manifest.json").write_text(json.dumps({"version":"1.1.0", "packagesDir":".lake/packages", "packages":[], "name":"transformFixture", "lakeDir":".lake"}))
        server.set_expected_peer_uid(os.geteuid())
        server.start()
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        inp = dict(name="Original", statement="theorem Original : ∀ n : Nat, n = 0 := by",
                   context="import Lean\n", condition="n < 3", arguments=["Nat.succ 2"])
        try:
            first = client_statement_transform(server.socket_path, inp, timeout_secs=60)
            assert first.get("status") == "ok", json.dumps(first, indent=2)
            second = client_statement_transform(server.socket_path, inp, timeout_secs=60)
            assert first["result"] == second["result"]
            assert len(first["helper_sha256"]) == 64
            assert first["environment"]["toolchain_sha256"]
            signature_input = dict(name="Original", statement="theorem Original : True := by",
                                   context="import Lean\n", fingerprint_node="Original")
            signature = client_statement_transform(server.socket_path, signature_input, timeout_secs=60)
            assert signature.get("status") == "ok", signature
            assert "root|Original|decl=Original" in signature["result"]["payload"]
            assert signature["helper_sha256"] != first["helper_sha256"]
            assert client_statement_transform(server.socket_path, signature_input, timeout_secs=60)["result"] == signature["result"]
            invalid = client_statement_transform(server.socket_path, {**inp, "arguments":[]}, timeout_secs=60)
            assert invalid.get("status") == "invalid_statement", invalid
            # Transport must route the public check command through this same op.
            env = {**os.environ, "TRELLIS_CHECKER_SOCKET":str(server.socket_path), "PYTHONPATH":str(ROOT)}
            result = subprocess.run(["python3", "-c", "from trellis.agent_check import main; raise SystemExit(main())",
                                     "lean-statement-transform", "--input-json", json.dumps(inp)],
                                    cwd=repo, env=env, text=True, capture_output=True, timeout=60)
            assert result.returncode == 0, result.stdout + result.stderr
            assert json.loads(result.stdout)["result"] == first["result"]
            log = server.log_path.read_text()
            assert '"statement_transform_cache_hit": true' in log or '"statement_transform_cache_hit":true' in log
            # A cached success must not authorize an unpinned execution context.
            for target in (repo, server.supervisor_repo):
                (target / "lean-toolchain").unlink()
            unpinned = client_statement_transform(server.socket_path, inp, timeout_secs=60)
            assert unpinned.get("status") == "invalid_statement", unpinned
            assert "pinned lean-toolchain" in unpinned["error"]
        finally:
            server.shutdown()
            thread.join(timeout=5)


@pytest.fixture
def signature_repo(tmp_path):
    """An isolated, standard-library-only Tablet; no shared fixture mutation."""
    (tmp_path / "Tablet").mkdir()
    (tmp_path / "lean-toolchain").write_text((FIXTURE / "lean-toolchain").read_text())
    (tmp_path / "lakefile.lean").write_text(
        "import Lake\nopen Lake DSL\npackage signatureFixture\nlean_lib Tablet\n")
    return tmp_path


def signature_lean(repo, *args):
    env = dict(os.environ)
    # Both direct Lean and `lake env lean` can import the compiled, isolated
    # Tablet modules without rebuilding or changing the shared fixture.
    env["LEAN_PATH"] = str(repo) + os.pathsep + env.get("LEAN_PATH", "")
    explicit = env.get("TRELLIS_FIXTURE_LEAN")
    if explicit:
        env["LEAN_SYSROOT"] = str(Path(explicit).resolve().parent.parent)
        command = [explicit]
    else:
        assert shutil.which("lake"), "pinned Lean fixture is required"
        command = ["lake", "env", "lean"]
    return subprocess.run([*command, *map(str, args)], cwd=repo, env=env,
                          text=True, capture_output=True, timeout=45)


def compile_signature_node(repo, node, source):
    path = repo / "Tablet" / f"{node}.lean"
    path.write_text(source)
    result = signature_lean(repo, "-o", path.with_suffix(".olean"), path)
    assert result.returncode == 0, result.stdout + result.stderr


def signature_payload(repo, name, statement, context, node="Original"):
    inp = dict(name=name, statement=statement, context=context, fingerprint_node=node)
    result = signature_lean(repo, "--run", SEMANTIC_SCRIPT, "--statement", json.dumps(inp))
    assert result.returncode == 0, result.stdout + result.stderr
    row = json.loads(result.stdout)
    assert row["status"] == "ok", row
    return row["result"]["payload"]


def compiled_payload(repo, node, principal):
    result = signature_lean(repo, "--run", SEMANTIC_SCRIPT, node, principal)
    assert result.returncode == 0, result.stdout + result.stderr
    prefix = f"FP\t{node}\t"
    rows = [line[len(prefix):] for line in result.stdout.splitlines() if line.startswith(prefix)]
    assert len(rows) == 1, result.stdout + result.stderr
    return rows[0]


@pytest.mark.parametrize("context,name,principal,statement,proof,ending", [
    ("import Lean\nnamespace Scope\n", "Original", "Scope.Original",
     "theorem Original : True := by", "  trivial\n", "end Scope\n"),
    ("import Lean\nnamespace Scope\n", "_root_.Original", "Original",
     "theorem _root_.Original : True := by", "  trivial\n", "end Scope\n"),
    ("import Lean\n", "Original", "Original",
     "theorem Original : ∀ xs : List Nat, match xs with | [] => True | n :: _ => n = n := by",
     "  intro xs\n  cases xs <;> simp\n", ""),
    ('import Lean\nnamespace Scope\nscoped notation "MyNat" => Nat\nend Scope\n'
     'open scoped Scope\nnamespace Active\nset_option autoImplicit false\n',
     "Original", "Active.Original", "theorem Original : ∀ n : MyNat, n = n := by",
     "  intro n\n  rfl\n", "end Active\n"),
])
def test_signature_fingerprint_matches_compiled_namespace_and_matcher_types(
        signature_repo, context, name, principal, statement, proof, ending):
    compile_signature_node(signature_repo, "Original", context + statement + "\n" + proof + ending)
    # This is the old, independently compiled correspondence authority. A
    # dormant signature must recover that exact identity without its proof.
    approved = compiled_payload(signature_repo, "Original", principal)
    observed = signature_payload(signature_repo, name, statement, context)
    assert observed == approved
    assert "||missing|" not in observed


def test_signature_fingerprint_catches_local_notation_negation_exploit(signature_repo):
    statement = "theorem Original : ¬ True := by"
    clean = "import Lean\n"
    shadowed = clean + 'local notation "True" => False\n'
    source = shadowed + statement + "\n  intro h\n  exact h\n"
    compile_signature_node(signature_repo, "Original", source)
    # It really compiles, but proves ¬False. Identical prescribed declaration
    # bytes and namespace do not make this the registered negation ¬True.
    expected = signature_payload(signature_repo, "Original", statement, clean)
    actual = signature_payload(signature_repo, "Original", statement, shadowed)
    assert actual != expected
    assert actual == compiled_payload(signature_repo, "Original", "Original")
    normalized = []
    for context in (clean, shadowed):
        process, row = derive(statement, context=context)
        assert process.returncode == 0, row
        normalized.append(signature_payload(
            signature_repo, "Original",
            f'theorem Original : {row["result"]["refutation_type"]} := by', context))
    # Normalizing temporary matchers must not erase context-dependent meaning.
    assert normalized[0] != normalized[1]
    wrong_meaning = signature_repo / "WrongMeaning.lean"
    wrong_meaning.write_text(source + "example : ¬ _root_.True := Original\n")
    result = signature_lean(signature_repo, wrong_meaning)
    assert result.returncode != 0 and "type mismatch" in result.stdout.lower()
    # A real proof edit and harmless FREE options preserve typed meaning.
    compile_signature_node(signature_repo, "Original", source.replace("exact h", "cases h"))
    assert compiled_payload(signature_repo, "Original", "Original") == actual
    assert signature_payload(signature_repo, "Original", statement,
                             shadowed + "set_option pp.universes true\nopen Nat\n") == actual


def test_signature_fingerprint_tracks_imported_definition_meaning(signature_repo):
    context = "import Tablet.Domain\n"
    statement = "theorem Original : bound = bound := by"
    source = context + statement + "\n  rfl\n"
    compile_signature_node(signature_repo, "Domain", "def bound : Nat := 1\n")
    compile_signature_node(signature_repo, "Original", source)
    approved = compiled_payload(signature_repo, "Original", "Original")
    assert signature_payload(signature_repo, "Original", statement, context) == approved
    # Both statement strings and both FREE contexts are unchanged; only the
    # imported definition differs, so comparing printed types would miss this.
    compile_signature_node(signature_repo, "Domain", "def bound : Nat := 2\n")
    compile_signature_node(signature_repo, "Original", source)
    changed = signature_payload(signature_repo, "Original", statement, context)
    assert changed != approved
    assert changed == compiled_payload(signature_repo, "Original", "Original")


def test_signature_fingerprint_relation_normalizes_legacy_matchers(signature_repo):
    proposition = "∀ xs : List Nat, match xs with | [] => True | n :: _ => n = n"
    process, derived = derive(f"theorem Original : {proposition} := by")
    assert process.returncode == 0, derived
    negative_statements = [
        f"theorem Original : ¬ ({proposition}) := by",
        f'theorem Original : {derived["result"]["refutation_type"]} := by',
    ]
    # A legacy source match elaborates with a private matcher; native output
    # closes that temporary declaration into portable recursors. Compare their
    # normalized negations while leaving approved correspondence hashes intact.
    normalized = []
    for statement in negative_statements:
        process, result = derive(statement)
        assert process.returncode == 0, result
        normalized.append(result["result"]["refutation_type"])
    payloads = [signature_payload(signature_repo, "Original",
                                 f"theorem Original : {ty} := by", "import Lean\n")
                for ty in normalized]
    assert payloads[0] == payloads[1]
    # Ask Lean itself to establish that neither normalization changed the
    # intended proposition; equality of two printer outputs is not enough.
    path = signature_repo / "NormalizedMeaning.lean"
    path.write_text("import Lean\nset_option smartUnfolding false\n" + "\n".join(
        f"example : ({ty}) = (¬ ¬ ({proposition})) := rfl" for ty in normalized))
    checked = signature_lean(signature_repo, path)
    assert checked.returncode == 0, checked.stdout + checked.stderr
