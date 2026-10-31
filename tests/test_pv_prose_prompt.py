from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
FIRST_REQUEST_FRAGMENT = (
    REPO_ROOT
    / "trellis"
    / "prompt_fragments"
    / "pv"
    / "worker"
    / "theorem_stating"
    / "12_first_request_decomposition.md"
)
COMMON_FAILURES_FRAGMENT = FIRST_REQUEST_FRAGMENT.with_name(
    "20_common_failure_modes.md"
)
AUTHOR_ASSUMPTION_FRAGMENT = (
    REPO_ROOT
    / "trellis"
    / "prompt_fragments"
    / "pv"
    / "worker"
    / "common"
    / "71_author_assumption.md"
)


def test_first_prose_request_authors_current_nodes_and_allows_prior_proofs() -> None:
    fragment = FIRST_REQUEST_FRAGMENT.read_text(encoding="utf-8")

    assert "for the current source and requirements" in fragment
    assert "reusing applicable prior statements and proofs where available" in fragment
    assert "from scratch" not in fragment
    assert "disposable placeholders" not in fragment
    assert "Setup seeds one placeholder scaffold" not in fragment
    assert "theorem <Node> : True" not in fragment


def test_prose_prompt_discloses_hook_naming_and_authoring_order_once() -> None:
    fragments = {
        "first": FIRST_REQUEST_FRAGMENT.read_text(encoding="utf-8"),
        "failures": COMMON_FAILURES_FRAGMENT.read_text(encoding="utf-8"),
        "authoring": AUTHOR_ASSUMPTION_FRAGMENT.read_text(encoding="utf-8"),
    }

    naming = "one CamelCase identifier beginning `RustValid`, without underscores"
    ordering = "validity definition as an ordinary node before staging"
    assert naming in fragments["failures"]
    assert sum(naming in text for text in fragments.values()) == 1
    assert ordering in fragments["first"]
    assert sum(ordering in text for text in fragments.values()) == 1
    assert "available Rust-validity/correspondence hook" not in fragments["authoring"]
    assert sum("Rust-docs corpus locator" in text for text in fragments.values()) == 1
    assert "`20_common_failure_modes.md`" not in fragments["first"]
    assert "pointer provenance" not in fragments["first"]
