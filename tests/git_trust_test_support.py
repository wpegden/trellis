"""Read-only native-install fixture discovery. Never executes Lake or Lean."""
from functools import lru_cache
import hashlib
import os
from pathlib import Path

import pytest

from trellis import git_trust as gt


def clean_install_environment(monkeypatch):
    for key in tuple(os.environ):
        if key in {'ELAN', 'ELAN_HOME', 'ELAN_TOOLCHAIN', 'LEAN', 'LEAN_SYSROOT',
                   'LAKE_OVERRIDE_LEAN'} or key.startswith('LD_'):
            monkeypatch.delenv(key)


@lru_cache(maxsize=1)
def audited_native_bin():
    # These subprocess tests require the exact audited distribution; unit
    # tests below exercise the same identity check with synthetic trust anchors.
    roots = (Path.home() / '.elan/toolchains').glob('*')
    def fingerprint(path):
        with path.open('rb') as source:
            result = hashlib.sha256()
            for chunk in iter(lambda: source.read(131072), b''):
                result.update(chunk)
            return result.hexdigest()
    for root in roots:
        try:
            if all(fingerprint(root / name) == digest
                   for name, digest in gt._AUDITED_NATIVE_INSTALL.items()):
                return root / 'bin'
        except OSError:
            continue
    pytest.skip('audited native distribution unavailable; no toolchain is downloaded or executed')


def native_lake_path(prefix):
    return f'{prefix}{os.pathsep}{audited_native_bin()}'
