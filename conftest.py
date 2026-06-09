"""Repo-wide pytest configuration.

Two pieces, both aimed at eliminating the per-pytest-run /tmp leak +
SSD wear that historically came from tests calling
``tempfile.mkdtemp()`` without cleanup and from
``materialize_project_runtime`` doing real binary copies into those
tempdirs:

1. **basetemp redirected to a same-filesystem cache.** pytest's default
   ``tmp_path`` lives under ``/tmp``, which is on a separate partition
   from the source tree on this host. Cross-filesystem destinations
   force ``shutil.copy2`` (used by ``materialize_project_runtime``) to
   actually copy the 76 MB kernel binary into every test's fixture
   tree, costing ~3 GB of writes per pytest run. Setting
   ``--basetemp`` to ``~/.cache/trellis-pytest/`` puts pytest's
   per-test scratch on the same filesystem as the source, which lets
   ``materialize_project_runtime`` use ``os.link`` (hard-link, see
   ``trellis/runtime_snapshot.py``) instead — zero bytes copied.

2. **``tempfile.mkdtemp`` redirected to ``tmp_path``.** Several test
   files (``test_runtime_bridge.py``, ``test_check.py``, the various
   ``test_checker_*`` and ``*_routing`` files) call
   ``tempfile.mkdtemp()`` directly with no cleanup. An autouse
   ``monkeypatch`` fixture redirects every such call to live under the
   test's ``tmp_path``, so pytest cleans them up automatically. Tests
   that pass an explicit ``dir=`` are respected.

Together this means a fresh pytest run writes only the small per-test
scratch (a few MB), all under ``~/.cache/trellis-pytest/``, which
pytest evicts on its own schedule. No /tmp pollution, no SSD-wasting
binary copies.
"""

from __future__ import annotations

import os
import tempfile
from pathlib import Path

import pytest


_DEFAULT_BASETEMP = Path.home() / ".cache" / "trellis-pytest"


def pytest_configure(config: pytest.Config) -> None:
    """Relocate pytest's basetemp to the user's same-filesystem cache.

    Skipped if the operator already supplied ``--basetemp`` on the
    command line (so CI / debugging workflows that pin a specific dir
    keep working).
    """
    if config.getoption("basetemp"):
        return
    base = Path(os.environ.get("TRELLIS_TEST_BASETEMP", str(_DEFAULT_BASETEMP)))
    base.mkdir(parents=True, exist_ok=True)
    # pytest reads `config.option.basetemp` when it constructs
    # `tmp_path_factory`; setting it here before collection is the
    # supported way to override the default.
    config.option.basetemp = str(base)


@pytest.fixture(autouse=True)
def _redirect_mkdtemp_to_tmp_path(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """Redirect ``tempfile.mkdtemp()`` calls without an explicit
    ``dir=`` to live under the per-test ``tmp_path``. Pytest cleans
    ``tmp_path`` automatically, so leaked tempdirs go away. Tests that
    pass ``dir=`` explicitly (a deliberate path choice) are
    respected."""
    real_mkdtemp = tempfile.mkdtemp
    counter = {"n": 0}

    def patched(suffix=None, prefix=None, dir=None):
        if dir is not None:
            return real_mkdtemp(suffix=suffix, prefix=prefix, dir=dir)
        counter["n"] += 1
        # Encode prefix/suffix so debugging traces can still see the
        # caller's hint without losing the per-call uniqueness pytest's
        # numeric suffix provides.
        name_parts = []
        if prefix:
            name_parts.append(str(prefix).rstrip("-_"))
        name_parts.append(f"mkdtemp{counter['n']}")
        if suffix:
            name_parts.append(str(suffix).lstrip("-_"))
        target = tmp_path / "-".join(name_parts)
        target.mkdir(parents=True, exist_ok=False)
        return str(target)

    monkeypatch.setattr(tempfile, "mkdtemp", patched)
