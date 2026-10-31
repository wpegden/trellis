"""Durable, human-ratified remediation of formally refuted PV goals.

Agents propose data. This controller validates and applies it; no agent owns the
state, goal collection, accepted source, or approval records.
"""
from __future__ import annotations

from copy import deepcopy
import hashlib
import json
from pathlib import Path, PurePosixPath

from trellis.json_io import save_json
from trellis.pv_revision import store

ROLES = {"audit", "reviewer", "worker", "scope", "aggregate_scope", "comment_scope"}


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def _text(value, label):
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{label} must be nonempty text")
    return value


def validate_human_reason(action, reason):
    if not isinstance(reason, str) or len(reason) > 20000:
        raise ValueError("Human reason must be text of at most 20000 characters")
    if action != "accept":
        _text(reason, "human reason")


def _list(value, label):
    if not isinstance(value, list):
        raise ValueError(f"{label} must be a list")
    return value


def _goals(value):
    seen = set()
    for goal in _list(value, "goals"):
        if not isinstance(goal, dict):
            raise ValueError("Each goal must be an object")
        ident = _text(goal.get("id"), "goal id")
        _text(goal.get("text"), "goal text")
        if ident in seen:
            raise ValueError("Duplicate goal id")
        seen.add(ident)
    return value


def _alternatives(value, *, worker=False):
    for item in _list(value, "alternatives"):
        if not isinstance(item, dict):
            raise ValueError("Alternative must be an object")
        _text(item.get("rationale"), "alternative rationale")
        _text(item.get("approach" if worker else "remedy"), "alternative approach")
        for evidence in _list(item.get("evidence"), "alternative evidence"):
            _text(evidence, "evidence")
        if worker and item.get("status") not in {"considered", "tested"}:
            raise ValueError("Implementation alternatives distinguish considered from tested")
        if worker and item["status"] == "tested" and not item["evidence"]:
            raise ValueError("Tested alternatives require evidence")


def validate_response(role, result):
    """Structural validation, also used by the shared artifact executor."""
    if role not in ROLES or not isinstance(result, dict):
        raise ValueError("Unknown revision role or non-object response")
    if "system_feedback" in result and not isinstance(result["system_feedback"], str):
        raise ValueError("system_feedback must be text")
    from trellis.pv_revision.notes import validate_notes
    validate_notes(role, result)
    if role == "audit":
        _text(result.get("summary"), "audit summary")
        _alternatives(result.get("alternatives"))
        from trellis.pv_revision.store import validate_invalidation
        validate_invalidation(result.get("invalidation", result.get("invalidate", result.get("invalidation_ids", []))))
        for disposition in _list(result.get("dispositions"), "dispositions"):
            _text(disposition.get("goal_id"), "goal_id")
            _text(disposition.get("reason"), "disposition reason")
            remedy = disposition.get("remedy")
            if remedy not in {"replace", "modify", "retire", "repair", "model_mismatch"}:
                raise ValueError("Unknown remedy")
            _goals(disposition.get("replacement_goals", []))
            _goals(disposition.get("surviving_obligations", []))
            _list(disposition.get("task_ids", []), "task_ids")
            if remedy in {"replace", "modify"} and not disposition.get("replacement_goals"):
                raise ValueError("Replacement requires explicit natural-language GOAL text")
            if remedy == "retire" and "surviving_obligations" not in disposition:
                raise ValueError("Retirement must explicitly inventory surviving conditional obligations")
            if remedy == "repair" and not disposition.get("task_ids"):
                raise ValueError("Source repair requires a task")
            if remedy not in {"replace", "modify"} and disposition.get("replacement_goals"):
                raise ValueError("Only requirement modification supplies replacement GOALs")
            if remedy != "retire" and disposition.get("surviving_obligations"):
                raise ValueError("Only retirement supplies surviving obligations")
        ids = set()
        for task in _list(result.get("tasks"), "tasks"):
            for key in ("id", "title", "authorization"):
                _text(task.get(key), f"task {key}")
            if task["id"] in ids:
                raise ValueError("Duplicate task id")
            ids.add(task["id"])
            _list(task.get("goal_ids"), "task goal_ids")
            for path in _list(task.get('evidence_paths', []), 'task evidence_paths'):
                _text(path, 'evidence path')
            checks = _list(task.get("checks"), "task checks")
            if not checks:
                raise ValueError("Source-repair tasks require independently executable checks")
            for check in checks:
                _text(check.get("id"), "check id")
                argv = _list(check.get("argv"), "check argv")
                if not argv or any(not isinstance(arg, str) or not arg for arg in argv):
                    raise ValueError("Check argv must be a nonempty string list")
                if "baseline_expected_exit" in check and (not isinstance(check["baseline_expected_exit"], int) or isinstance(check["baseline_expected_exit"], bool)):
                    raise ValueError("Expected baseline exit must be an integer")
                if not isinstance(check.get("expected_exit", 0), int) or isinstance(check.get("expected_exit", 0), bool):
                    raise ValueError("Expected check exit must be an integer")
                for path in _list(check.get("harness_paths", []), "check harness_paths"):
                    _text(path, "harness path")
                    if PurePosixPath(path).is_absolute() or ".." in PurePosixPath(path).parts or path == ".":
                        raise ValueError("Check harness path must be a confined relative file")
            store.harness_paths([task])
            if "allowed_paths" in task and "paths" not in task:
                task["paths"] = task["allowed_paths"]
            if "paths" in task:
                for path in _list(task["paths"], "paths"):
                    _text(path, "path")
                    if PurePosixPath(path).is_absolute() or ".." in PurePosixPath(path).parts or path in {".", ""}:
                        raise ValueError("Task path must be a confined relative path")
    elif role == "worker":
        _text(result.get("summary"), "worker summary")
        _alternatives(result.get("alternatives"), worker=True)
        if result.get("outcome") not in {"complete", "failed", "exhausted"}:
            raise ValueError("Worker needs complete, failed, or exhausted outcome")
    elif role == "reviewer":
        if result.get("action") not in {"work", "complete", "reaudit", "fail"}:
            raise ValueError("Unknown reviewer action")
        _text(result.get("reason"), "reviewer reason")
        if result["action"] == "work":
            _text(result.get("task_id"), "task id")
    else:
        if result.get("decision") not in {"pass", "fail"}:
            raise ValueError("Scope needs pass or fail decision")
        _text(result.get("reason"), "scope reason")
        _text(result.get("binding"), "scope binding")
        if role != "comment_scope":
            _text(result.get("alternatives_assessment"), "implementation alternatives assessment")
        else:
            for note in _list(result.get("editorial_notes", []), "editorial notes"):
                _text(note, "editorial note")
    return result


def _manifest(path):
    from trellis.pv_revision import store
    return store.source_manifest(path)


def _copy(source, dest):
    from trellis.pv_revision import store
    return store.copy_source_tree(source, dest)


def initialize(root, source, goals, negated_goals, *, config_template=None, handoff=None, tools=None):
    root, source = Path(root), Path(source)
    if root.exists():
        raise ValueError(f"Revision directory already exists: {root}")
    collection = deepcopy(goals if isinstance(goals, dict) else {"context": "", "goals": goals})
    if not isinstance(collection.get("context", ""), str):
        raise ValueError("Shared goal context must be text")
    goals = deepcopy(_goals(collection["goals"]))
    known = {goal["id"] for goal in goals}
    negated = [item["goal_id"] if isinstance(item, dict) else item for item in negated_goals]
    if not negated or len(set(negated)) != len(negated) or not set(negated) <= known:
        raise ValueError("Refuted goals must be unique members of the goal collection")
    root.mkdir(parents=True)
    _copy(source, root / "sources/original")
    manifest = _manifest(root / "sources/original")
    state = {"version": 1, "status": "active", "phase": "audit", "sequence": 0, "round": 0,
             "next": {"role": "audit"}, "original_collection": collection, "original_goals": goals, "proposed_goals": goals, "tools": tools or {},
             "negated_goals": negated, "handoff": handoff or {}, "original_manifest": manifest,
             "source": "sources/original", "source_manifest": manifest, "plan": None,
             "tasks": {}, "history": [], "attempts": [], "human_decisions": [], "feedback": ""}
    if config_template:
        config = json.loads(Path(config_template).read_text())
        config["repo_path"] = str(root.resolve())
        save_json(root / "trellis.config.json", config)
        from trellis.git_ops import init_repo
        from trellis.checking import write_scripts
        init_repo(root)
        write_scripts(root, root / ".trellis")
    save_json(root / "state.json", state)
    return state


class Driver:
    def __init__(self, root, agents=None, checker=None):
        self.root = Path(root).resolve()
        self.path = self.root / "state.json"
        self.state = json.loads(self.path.read_text())
        if self.state.get("version") != 1:
            raise ValueError("Unknown revision state version")
        if agents is None:
            from trellis.pv_revision.agents import Agents
            agents = Agents()
        self.agents, self.checker = agents, checker

    def save(self):
        save_json(self.path, self.state)

    def _current(self):
        if _manifest(self.root / "sources/original") != self.state["original_manifest"]:
            raise ValueError("Original source changed; revision evidence is invalid")
        if _manifest(self.root / self.state["source"]) != self.state["source_manifest"]:
            raise ValueError("Accepted candidate source changed; approvals are stale")
        from trellis.pv_revision.code_edits import references
        references(self.root, self.state)
        if self.state.get("comment_amendment"):
            from trellis.pv_revision.comment_amendments import validate
            validate(self.root, self.state)

    def _binding(self, state, task=None, candidate=None, evidence=None):
        return digest({"round": state["round"], "plan": state["plan"], "task": task,
                       "base": state["source_manifest"], "candidate": candidate, "evidence": evidence,
                       "goals": state["proposed_goals"], "collection": state.get("proposed_collection"),
                       "aggregate_checks": state.get("aggregate_checks", {}),
                       "completed_attempts": state["attempts"]})

    def _reaudit(self, state, reason):
        state.update(status="active", phase="audit", next={"role": "audit"}, feedback=reason,
                     round=state["round"] + 1, tasks={}, plan=None)
        state.update(source="sources/original", source_manifest=deepcopy(state["original_manifest"]),
                     proposed_goals=deepcopy(state["original_goals"]),
                     proposed_collection=deepcopy(state["original_collection"]))
        state.pop("aggregate_scope", None)
        state.pop("aggregate_checks", None)
        state.pop("human_package", None)
        state.pop("review_repackaging", None)
        state.pop("pending_attempt", None)
        state.pop("comment_amendment", None)

    def _audit(self, state, result):
        dispositions = result["dispositions"]
        ids = [item["goal_id"] for item in dispositions]
        known = {goal["id"] for goal in state["original_goals"]}
        if len(ids) != len(set(ids)):
            raise ValueError("Audit must dispose of each goal at most once")
        if not set(state["negated_goals"]) <= set(ids):
            raise ValueError("Audit must dispose of every refuted goal exactly once")
        if not set(ids) <= known:
            raise ValueError("Audit dispositions must identify existing goals")
        task_ids = {task["id"] for task in result["tasks"]}
        for task in result["tasks"]:
            if not task["goal_ids"] or not set(task["goal_ids"]) <= known:
                raise ValueError("Task must identify known goals")
        proposed = {goal["id"]: deepcopy(goal) for goal in state["original_goals"]}
        introduced = set()
        for disposition in dispositions:
            refs = disposition.get("task_ids", [])
            if not set(refs) <= task_ids:
                raise ValueError("Disposition references unknown repair task")
            for ident in refs:
                task = next(t for t in result["tasks"] if t["id"] == ident)
                if disposition["goal_id"] not in task["goal_ids"]:
                    raise ValueError("Repair task does not cover disposition goal")
            if disposition["remedy"] not in {"repair", "model_mismatch"}:
                del proposed[disposition["goal_id"]]
            for goal in disposition.get("replacement_goals", []) + disposition.get("surviving_obligations", []):
                if goal["id"] in introduced or (goal["id"] in known and goal["id"] != disposition["goal_id"]):
                    raise ValueError("New goal id collides with another goal")
                introduced.add(goal["id"])
                proposed[goal["id"]] = deepcopy(goal)
        state["proposed_goals"] = list(proposed.values())
        state["proposed_collection"] = dict(state["original_collection"], goals=state["proposed_goals"])
        from trellis.pv_revision.store import validate_invalidation
        baseline_nodes = state.get("handoff", {}).get("baseline", {}).get("nodes")
        known_nodes = (set(baseline_nodes) | known | set(proposed)) if baseline_nodes is not None else None
        validate_invalidation(result.get("invalidation", result.get("invalidate", result.get("invalidation_ids", []))), known_nodes)
        from trellis.pv_revision.notes import bind_notes
        bind_notes("audit", result, state, root=self.root)
        state["plan"] = deepcopy(result)
        state["tasks"] = {task["id"]: {"authorization": deepcopy(task), "status": "pending", "attempts": []}
                          for task in result["tasks"]}
        if any(item["remedy"] == "model_mismatch" for item in dispositions):
            state.update(status="requires_modeling_review", phase="modeling_review")
        else:
            state.update(phase="implementation", next={"role": "reviewer"})

    def _apply(self, state, task, result):
        validate_response(task["role"], result)
        role = task["role"]
        result = deepcopy(result)
        if role == "comment_scope":
            from trellis.pv_revision.comment_amendments import validate
            validate(self.root, state)
            if result["binding"] != state["comment_amendment"]["binding"]:
                raise ValueError("Stale focused comment verdict")
            state["comment_amendment"]["review"] = result
            state.update(status="awaiting_human", phase="human_gate")
            state["feedback"] = "" if result["decision"] == "pass" else "Comment review failed: " + result["reason"]
        elif role == "audit":
            self._audit(state, result)
        elif role == "reviewer":
            action = result["action"]
            if action == "reaudit":
                self._reaudit(state, result["reason"])
            elif action == "fail":
                state.update(status="failed", failure=result["reason"])
            elif action == "work":
                selected = state["tasks"].get(result["task_id"])
                if not selected or selected["status"] == "complete":
                    raise ValueError("Reviewer must select an unfinished authorized task")
                state["next"] = {"role": "worker", "task_id": result["task_id"]}
            else:
                if any(item["status"] != "complete" for item in state["tasks"].values()):
                    raise ValueError("Failed, exhausted, or unfinished tasks cannot count as success")
                state["aggregate_checks"] = {}
                for ident, selected in state["tasks"].items():
                    last = next(item for item in reversed(state["attempts"])
                                if item["task_id"] == ident and item["round"] == state["round"] and item["accepted"])
                    checks = self.checker(self.root / state["source"], {
                        "id": f"aggregate-{state['sequence']}-{ident}", "base_source": last["base_source"],
                        "authorization": selected["authorization"]}) if self.checker else {"ok": False}
                    state["aggregate_checks"][ident] = checks
                    if checks.get("ok") is not True:
                        selected["status"] = "pending"
                state.setdefault("aggregate_check_history", []).append({
                    "sequence": state["sequence"], "round": state["round"],
                    "source_manifest": deepcopy(state["source_manifest"]),
                    "checks": deepcopy(state["aggregate_checks"])})
                if any(item["status"] != "complete" for item in state["tasks"].values()):
                    state["feedback"] = "Final candidate regression checks failed; inspect aggregate_checks and repair the reopened tasks."
                    return
                state.update(phase="aggregate_scope", next={"role": "aggregate_scope"})
        elif role == "worker":
            attempt = deepcopy(state["pending_attempt"])
            attempt.update(response=deepcopy(result), manifest=_manifest(self.root / attempt["source"]))
            attempt["authorization"] = deepcopy(state["tasks"][task["task_id"]]["authorization"])
            if self.checker:
                attempt["checks"] = self.checker(self.root / attempt["source"], deepcopy(attempt))
            attempt["binding"] = self._binding(state, state["tasks"][task["task_id"]]["authorization"], attempt["manifest"],
                                               {"response": attempt["response"], "checks": attempt.get("checks")})
            state["pending_attempt"] = attempt
            state.update(phase="task_scope", next={"role": "scope", "task_id": task["task_id"]})
        elif role == "scope":
            attempt = state["pending_attempt"]
            candidate = _manifest(self.root / attempt["source"])
            selected = state["tasks"][task["task_id"]]
            expected = self._binding(state, selected["authorization"], candidate,
                                     {"response": attempt["response"], "checks": attempt.get("checks")})
            if result["binding"] != expected or attempt["binding"] != expected:
                raise ValueError("Stale task scope verdict")
            from trellis.pv_revision.store import changed_paths
            paths = selected["authorization"].get("paths")
            changed = changed_paths(state["source_manifest"], candidate)
            if paths is not None and any(not any(p == allowed or p.startswith(allowed.rstrip("/") + "/") for allowed in paths) for p in changed):
                if result["decision"] == "pass":
                    raise ValueError("Patch exceeds enforceable task paths")
            if result["decision"] == "pass" and attempt["response"]["outcome"] == "complete" and attempt.get("checks", {}).get("ok") is not True:
                raise ValueError("Task acceptance requires successful independent regression checks")
            accepted = result["decision"] == "pass" and attempt["response"]["outcome"] == "complete"
            from trellis.pv_revision.notes import bind_notes
            bind_notes(role, result, state, root=self.root, before=attempt["base"], after=candidate,
                       before_source=attempt["base_source"], after_source=attempt["source"])
            attempt["scope"] = deepcopy(result)
            attempt["accepted"] = accepted
            state["attempts"].append(deepcopy(attempt))
            selected["attempts"].append(attempt["id"])
            selected["status"] = "complete" if accepted else "pending"
            if accepted:
                state.update(source=attempt["source"], source_manifest=candidate)
            del state["pending_attempt"]
            state.update(phase="implementation", next={"role": "reviewer"})
        else:
            expected = self._binding(state)
            if result["binding"] != expected:
                raise ValueError("Stale aggregate scope verdict")
            if any(item["status"] != "complete" for item in state["tasks"].values()):
                raise ValueError("Aggregate scope cannot accept unfinished work")
            from trellis.pv_revision.notes import bind_notes
            bind_notes(role, result, state, root=self.root, before=state["original_manifest"], after=state["source_manifest"],
                       before_source="sources/original", after_source=state["source"])
            if result["decision"] == "fail":
                self._reaudit(state, result["reason"])
            else:
                state["aggregate_scope"] = deepcopy(result)
                state.update(status="awaiting_human", phase="human_gate")
                state["human_package"] = self._package(state)

    def _package(self, state):
        from trellis.pv_revision.store import build_review_package_from_state
        return build_review_package_from_state(self.root, state)

    def context(self, task):
        if task["role"] == "comment_scope":
            from trellis.pv_revision.comment_amendments import implementation_base, validate
            base = validate(self.root, self.state)
            checked = implementation_base(self.root, base)
            return {key: deepcopy(self.state[key]) for key in ("sequence", "source", "tools", "proposed_collection", "human_decisions")} | {
                "binding": self.state["comment_amendment"]["binding"],
                "handoff": {"reference_paths": self.state.get("handoff", {}).get("reference_paths", [])},
                "prior_review": str(self.root / "reviews" / base["digest"] / "review.json"),
                "prior_source": str(self.root / base["source"]),
                "implementation_review": str(self.root / "reviews" / checked["digest"] / "review.json"),
                "implementation_source": str(self.root / checked["source"]),
                "prior_comment_review": deepcopy(base.get("comment_amendment", {}).get("review")),
                "candidate_source": str(self.root / self.state["source"]),
                "comment_amendment": deepcopy(self.state["comment_amendment"]),
                "changes": store.tree_diff(self.root / base["source"], self.root / self.state["source"]),
                "cumulative_changes": store.tree_diff(self.root / checked["source"], self.root / self.state["source"]),
            }
        state = deepcopy(self.state)
        state["binding"] = self._binding(state)
        if task["role"] == "scope":
            state["binding"] = state["pending_attempt"]["binding"]
        return state

    def step(self):
        if self.state["status"] != "active":
            return self.state
        self._current()
        task = deepcopy(self.state["next"])
        self.state["sequence"] += 1
        sequence = self.state["sequence"]
        record_dir = self.root / "requests" / f"{sequence:05d}-{task['role']}"
        record_dir.mkdir(parents=True)
        if task["role"] == "worker":
            ident = f"attempt-{sequence:05d}"
            source = f"sources/{ident}"
            _copy(self.root / self.state["source"], self.root / source)
            self.state["pending_attempt"] = {"id": ident, "source": source, "base": self.state["source_manifest"],
                                             "task_id": task["task_id"], "round": self.state["round"],
                                             "base_source": str(self.root / self.state["source"])}
            task["writable_source"] = str(self.root / source)
        self.save()  # Recovery can retry a burst; immutable earlier attempts survive.
        context = self.context(task)
        save_json(record_dir / "context.json", {"state": context, "task": task})
        result = None
        error = ""
        try:
            result = self.agents(self.root, context, task)
            save_json(record_dir / "response.json", result)
            self._current()
            candidate = deepcopy(self.state)
            candidate["feedback"] = ""
            validate_response(task["role"], result)
            if result.get("system_feedback", "").strip():
                candidate.update(status="halted", feedback=result["system_feedback"],
                                 halt_reason="Agent reported tooling or process feedback requiring operator review")
            else:
                self._apply(candidate, task, result)
            candidate["consecutive_response_errors"] = 0
            self.state = candidate
        except Exception as exc:
            error = str(exc) or type(exc).__name__
            self.state["feedback"] = error
            self.state["consecutive_response_errors"] = self.state.get("consecutive_response_errors", 0) + 1
            from trellis.runtime.bridge import any_halt_marker_present
            if any_halt_marker_present(self.root.with_name(self.root.name + "-runtime")):
                self.state.update(status="halted", halt_reason=error)
            elif self.state["consecutive_response_errors"] >= 3:
                self.state.update(status="halted", halt_reason="Three consecutive invalid responses or failed bursts; inspect preserved evidence before retrying")
            save_json(record_dir / "error.json", {"error": error, "response": result})
        self.state["history"].append({"sequence": sequence, "task": task, "response": result, "error": error,
                                      "evidence": str(record_dir.relative_to(self.root))})
        if self.state["status"] == "awaiting_human" and task["role"] == "comment_scope":
            self.state["human_package"] = self._package(self.state)
        self.save()
        if self.state["status"] == "awaiting_human":
            from trellis.pv_revision.store import write_review_package
            write_review_package(self.root, self.state["human_package"])
            save_json(self.root / "human-package.json", self.state["human_package"])
        return self.state

    def decide(self, action, *, digest, reason="", amendment=None, code_edits=None, comment_only=False):
        validate_human_reason(action, reason)
        self._current()
        if self.state["status"] != "awaiting_human":
            raise ValueError("Revision is not awaiting human ratification")
        package = self._package(self.state)
        if package["digest"] != digest or package != self.state["human_package"]:
            raise ValueError("Human decision refers to a stale candidate")
        if action not in {"accept", "deny", "amend"}:
            raise ValueError("Unknown human decision")
        if code_edits is not None and not isinstance(code_edits, list):
            raise ValueError("Code amendments must be a list")
        if code_edits and action != "amend":
            raise ValueError("Edited code must be sent as a request for amendment")
        if amendment is not None and not isinstance(amendment, str):
            raise ValueError("Human amendment must be text")
        if action == "amend" and not code_edits:
            _text(amendment, "human amendment")
        if type(comment_only) is not bool or (comment_only and (action != "amend" or not code_edits)):
            raise ValueError("Comment-only review requires exact code edits in an amendment")
        if action == "accept" and not package["scope_passed"]:
            raise ValueError("This candidate has not passed scope review; request an amendment")
        if comment_only:
            from trellis.pv_revision.comment_amendments import validate_edits
            validate_edits(self.root, package, code_edits)
        edits = None
        if code_edits:
            from trellis.pv_revision.code_edits import save
            edits = save(self.root, package, code_edits)
        candidate = deepcopy(self.state)
        candidate["human_decisions"].append({"action": action, "digest": digest, "reason": reason, "amendment": amendment})
        if action == "accept" and package['schema'] == store.REVIEW_SCHEMA:
            candidate['human_decisions'][-1].update(
                ratified_candidate_digest=package['candidate_digest'],
                attached_evidence_digest=package['evidence_digest'], evidence_ratified=False)
        if edits:
            candidate["human_decisions"][-1]["code_edits"] = edits
        if action == "accept":
            candidate.update(status="accepted_for_modeling", phase="ratified")
        elif action == "deny":
            candidate.update(status="denied", phase="human_gate")
        else:
            feedback = f"Human amendment: {amendment or 'Use the requested code changes.'}\nReason: {reason}"
            if edits:
                feedback += f"\nExact requested code changes: {edits['path']}"
            if comment_only:
                from trellis.pv_revision.comment_amendments import prepare
                candidate["human_decisions"][-1]["comment_only"] = True
                candidate = prepare(self.root, candidate, package, edits)
            else:
                self._reaudit(candidate, feedback)
        self.state = candidate
        self.save()
        return self.state

    def redirect_comment_amendment(self, *, digest, reason, apply=False, state_digest=None):
        """Redirect an untouched, paused full audit of an exact human amendment."""
        from trellis.pv_revision.comment_amendments import base_package, prepare, validate_edits
        self._current()
        _text(reason, 'comment redirect reason')
        if not (self.root / 'pause').exists():
            raise ValueError('Pause the revision controller before redirecting an amendment')
        state = self.state
        if (state['status'] != 'active' or state['phase'] != 'audit' or state['next'] != {'role': 'audit'}
                or state['source'] != 'sources/original' or state['plan'] is not None or state['tasks']
                or state.get('pending_attempt') or state.get('comment_amendment')):
            raise ValueError('Only an untouched full-audit amendment can be redirected')
        decision = state['human_decisions'][-1] if state['human_decisions'] else {}
        if decision.get('action') != 'amend' or decision.get('digest') != digest or not decision.get('code_edits'):
            raise ValueError('Redirect requires the latest exact human amendment')
        package = base_package(self.root, digest)
        for key in ('attempts', 'aggregate_check_history', 'original_manifest', 'original_goals', 'original_collection'):
            if state.get(key) != package.get(key):
                raise ValueError('Revision evidence advanced since this human amendment')
        from trellis.pv_revision.code_edits import load
        validate_edits(self.root, package, load(self.root, decision['code_edits'])['edits'])
        before = store.digest(state)
        result = dict(state_digest=before, base_review=digest, code_edits=decision['code_edits'], applied=apply)
        if apply:
            if state_digest != before or store._read(self.path) != state:
                raise ValueError('Revision changed; repeat the comment redirect preview')
            candidate = prepare(self.root, state, package, decision['code_edits'])
            candidate.setdefault('operator_actions', []).append(dict(action='redirect_comment_amendment', reason=reason,
                after_sequence=state['sequence'], from_state_digest=before, base_review=digest))
            # Preserve the original human decision and abandoned request evidence.
            store._write(self.root / 'operator-evidence' / (before + '.json'), state, immutable=True)
            self.state = candidate
            self.save()
        return result

    def repackage_review(self, *, digest, separation_reason, apply=False, state_digest=None):
        """Upgrade a pending legacy review without rerunning or approving it."""
        self._current()
        _text(separation_reason, 'production/evidence separation review')
        if self.state['status'] != 'awaiting_human' or self.state['phase'] != 'human_gate':
            raise ValueError('Only a pending human review can be repackaged')
        old = self._package(self.state)
        if old != self.state.get('human_package') or old['digest'] != digest:
            raise ValueError('Review package changed; repeat the repackage preview')
        if old['schema'] != store.LEGACY_REVIEW_SCHEMA or not old['scope_passed']:
            raise ValueError('Repackaging requires a scope-passed legacy review')
        if store._read(self.root / 'reviews' / digest / 'review.json') != old:
            raise ValueError('Stored review evidence changed')
        before = store.digest(self.state)
        candidate = deepcopy(self.state)
        candidate['review_repackaging'] = {'from_digest': digest,
            'from_candidate_digest': old['candidate_digest'], 'from_state_digest': before,
            'rule': 'declared-new-evidence-files/v1', 'separation_reason': separation_reason}
        package = store.build_review_package_from_state(self.root, candidate, schema=store.REVIEW_SCHEMA)
        candidate['human_package'] = package
        result = {'from_digest': digest, 'digest': package['digest'], 'state_digest': before,
                  'candidate_digest': package['candidate_digest'], 'evidence_digest': package['evidence_digest'],
                  'production_changes': [row['path'] for row in package['aggregate_diff']['changes']],
                  'evidence_files': [row['relative_path'] for row in package['evidence']['source_manifest']['files']],
                  'scope_passed': package['scope_passed'], 'status': candidate['status'], 'applied': apply}
        if apply:
            if state_digest != before or store._read(self.path) != self.state:
                raise ValueError('Revision state changed; repeat the repackage preview')
            store.write_review_package(self.root, package)
            # Immutable artifacts precede the atomic state publication. The old
            # package remains intact; no human decision, source or verdict changes.
            self.state = candidate
            self.save()
            save_json(self.root / 'human-package.json', package)
        return result

    def recover(self, *, reason, reaudit=False):
        """Explicit operator recovery preserves failed evidence and approvals."""
        _text(reason, "recovery reason")
        self._current()
        allowed = {"halted", "failed", "requires_modeling_review", "denied"} if reaudit else {"halted"}
        if self.state["status"] not in allowed:
            raise ValueError("This revision requires a different recovery action")
        previous = {"status": self.state["status"], "feedback": self.state.get("feedback"),
                    "halt_reason": self.state.get("halt_reason")}
        self.state.setdefault("operator_actions", []).append(
            {"action": "reaudit" if reaudit else "retry", "reason": reason,
             "after_sequence": self.state["sequence"], "previous": previous})
        if reaudit:
            self._reaudit(self.state, f"Operator requested a new audit: {reason}")
        else:
            self.state.update(status="active", feedback=f"Operator recovery: {reason}")
        self.state["consecutive_response_errors"] = 0
        self.state.pop("halt_reason", None)
        self.save()
        return self.state
