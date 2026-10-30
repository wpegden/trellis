"""Select interpreter-free provider tools inside compiler-confined bursts."""
from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess

from trellis.agents.host_files import atomic_text, provider_directory
from trellis.config import SandboxConfig
from trellis.host_runtime import worker_path_env
from trellis.sandbox import _MANAGED_CHECK_ROLES, wrap_command


def direct_tool_catalog(config, burst_home: Path, work_dir: Path, *, role='worker') -> Path:
    """Preserve the effective provider catalog, changing only tool exposure.

    Catalog tool_mode takes precedence over feature flags in Codex. Its V8
    host requires executable anonymous memory, which our compiler boundary
    intentionally forbids. Direct tools run under the same boundary.
    """
    root_args, overrides = [], []
    # Honor catalog/provider configuration passed to this burst, just as exec
    # does. Model/effort and exec-only flags do not affect catalog discovery.
    extra = iter(config.extra_args or [])
    for arg in extra:
        if arg in ('-c', '--config'):
            overrides.extend([arg, next(extra, '')])
        elif arg.startswith('--config='):
            overrides.append(arg)
        elif arg in ('-p', '--profile', '--config-profile'):
            root_args.extend([arg, next(extra, '')])
        elif arg.startswith(('--profile=', '--config-profile=')) or arg == '--ignore-user-config':
            root_args.append(arg)
    command = ['codex', *root_args, 'debug', 'models', *overrides]
    try:
        # Configuration is agent-writable and may invoke credential helpers.
        # Even metadata discovery must run inside the production boundary.
        if not (os.environ.get('TRELLIS_COMPILER_DATA_VIEW') or os.environ.get('TRELLIS_COMPILER_BWRAP')):
            raise ValueError('compiler confinement is required for catalog export')
        if role not in _MANAGED_CHECK_ROLES:
            raise ValueError(f'compiler confinement does not support role {role!r}')
        with provider_directory(burst_home) as (directory, fd):
            path = worker_path_env(burst_home)
            command = wrap_command(command, sandbox=SandboxConfig(enabled=True, backend='bwrap'),
                                   work_dir=work_dir, burst_home=burst_home, role=role,
                                   git_preflight_path=path)
            result = subprocess.run(command, capture_output=True, text=True, timeout=30, cwd=work_dir,
                                    env=dict(os.environ, PATH=path))
            if result.returncode:
                diagnostic = result.stderr.strip()[-2000:]
                raise ValueError(f'catalog export failed (exit {result.returncode}): {diagnostic or "no diagnostic output"}')
            catalog = json.loads(result.stdout)
            models = catalog['models']
            if not isinstance(models, list) or not models or not all(
                    isinstance(model, dict) and isinstance(model.get('slug'), str) for model in models):
                raise ValueError('invalid model catalog')
            if config.model and not any(model['slug'] == config.model for model in models):
                raise ValueError(f'model {config.model!r} is absent from the provider catalog')
            for model in models:
                model['tool_mode'] = 'direct'
            return atomic_text(directory, fd, 'trellis-direct-models.json', json.dumps(catalog) + '\n')
    except (OSError, subprocess.TimeoutExpired, ValueError, KeyError, TypeError) as error:
        raise RuntimeError(f'Cannot prepare Codex direct tools for compiler confinement: {error}') from error
