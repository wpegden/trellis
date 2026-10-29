"""The shared native goal-collection input contract."""
from pathlib import Path
import json


def read_goal_collection(goal_file: Path) -> tuple[str, dict]:
    goal_raw = goal_file.read_text()
    try:
        collection = json.loads(goal_raw)
        goals = collection["goals"]
    except (json.JSONDecodeError, KeyError, TypeError) as error:
        raise ValueError("Supply a JSON goal collection with explicit IDs and complete text; shared context may be included") from error
    if not isinstance(goals, list) or any(not isinstance(g, dict) or not isinstance(g.get("id"), str)
                                          or not g["id"].strip() or not isinstance(g.get("text"), str) for g in goals):
        raise ValueError("Each goal needs an explicit nonempty string ID and complete text")
    if not goals or len({g["id"] for g in goals}) != len(goals) or any(not g["text"].strip() for g in goals):
        raise ValueError("Supply distinct, nonempty goals")
    context = collection.get("context", "")
    if not isinstance(context, str):
        raise ValueError("Shared goal context must be text")
    return goal_raw, collection


def verify_inventory(preparation: Path, native: Path) -> dict:
    """Check all supplied identities, including explicit audited declines."""
    _, collection = read_goal_collection(preparation / "goals.json")
    state = json.loads((preparation / ".trellis/modeling/state.json").read_text())
    supplied = [goal["id"] for goal in collection["goals"]]
    declined = state.get("declined", {})
    if not isinstance(declined, dict) or not set(declined).issubset(supplied):
        raise ValueError("Preparation reports goal declines outside the supplied inventory")
    expected = [goal for goal in supplied if goal not in declined]
    for file in ("pv_tablet.json", "trellis.config.json"):
        data = json.loads((native / file).read_text())
        actual = (data["pv_tablet"] if file == "trellis.config.json" else data)["verification_targets"]
        if actual != expected:
            raise ValueError(f"Native {file} target inventory differs from the supplied goals and recorded declines")
    if (native / "GOAL.md").read_bytes() != (preparation / "GOAL.md").read_bytes():
        raise ValueError("Native export changed the generated goal document")
    return {"supplied": supplied, "registered": expected, "declined": declined}
