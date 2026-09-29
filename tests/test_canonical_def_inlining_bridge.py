"""End-to-end sanity that the bridge prompt loader resolves the
canonical/<NAME>.md fragments to the project's actual lane rubrics.

This is the production-like complement to the kernel-side
`canonical_def_inlining_sanity` integration test: that test verifies
the kernel emits the right fragment paths in `prompt_fragments[]`, but
it doesn't catch fragment-loading bugs (escape-from-root, missing
files, broken symlinks, etc.). This test exercises the actual bridge
loader and checks the rendered text contains the canonical-def
content."""

from __future__ import annotations

import pytest

from trellis.runtime.bridge_prompts import (
    PROMPT_FRAGMENT_ROOT,
    _render_prompt_fragment,
    _resolve_prompt_fragment,
)


_CANONICAL_FRAGMENTS = {
    "canonical/SUBSTANTIVENESS.md": ("# Substantiveness", "**substantive**"),
    "canonical/FAITHFULNESS.md": ("# Paper-Faithfulness", "**paper-faithful**"),
    "canonical/CORRESPONDENCE.md": ("# Correspondence", "**corresponding**"),
    "canonical/SOUNDNESS.md": ("# Soundness", "**sound**"),
}


@pytest.mark.parametrize("fragment_id,expected", list(_CANONICAL_FRAGMENTS.items()))
def test_canonical_fragment_resolves_to_real_file(
    fragment_id: str, expected: tuple[str, str]
) -> None:
    """The bridge loader's `_resolve_prompt_fragment` must accept the
    `canonical/<NAME>.md` path. Symlinks pointing OUT of the
    prompt_fragments root would fail the security check (`root not in
    path.parents`), so this test pins that the canonical files live
    inside `prompt_fragments/canonical/` (not at the project root)."""
    path = _resolve_prompt_fragment(fragment_id)
    root = PROMPT_FRAGMENT_ROOT.resolve()
    assert root in path.parents, (
        f"resolved {fragment_id} to {path}, which is not under {root} — "
        f"the bridge loader will reject it. Move the canonical content "
        f"inside prompt_fragments/canonical/ rather than symlinking out."
    )
    assert path.is_file(), f"{fragment_id} resolved to {path} which is not a regular file"


@pytest.mark.parametrize("fragment_id,expected", list(_CANONICAL_FRAGMENTS.items()))
def test_canonical_fragment_renders_with_expected_content(
    fragment_id: str, expected: tuple[str, str]
) -> None:
    """The rendered text must contain the canonical lane title and
    the bolded predicate (e.g. `**substantive**`). Catches accidental
    truncation, empty-file replacement, or wrong-target symlink."""
    title, predicate = expected
    rendered = _render_prompt_fragment(fragment_id, {})
    assert title in rendered, f"{fragment_id} missing title '{title}'; rendered: {rendered[:200]}"
    assert predicate in rendered, (
        f"{fragment_id} missing bolded predicate '{predicate}'; rendered: {rendered[:200]}"
    )
