"""Codex catalog admission for immutable paper launches.

This checks advertised model/effort support, not inference, quota or future
account availability. The caller provisions an ordinary sandboxed
workspace and isolated provider home, and installs its restored launch
environment before calling. Discovery uses the production sandbox.
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import selectors
import shutil
import signal
import stat
import subprocess
import sys
import time
import tomllib

from trellis.host_runtime import worker_path_env

SCHEMA = "trellis-codex-model-preflight/v1"
MAX_OUTPUT_BYTES = 8 * 1024 * 1024
MAX_CONFIGURATION_BYTES = 8 * 1024 * 1024
MAX_EXECUTABLE_BYTES = 512 * 1024 * 1024
PROBE_TIMEOUT_SECONDS = 30
_ROLES = ("worker", "easy_worker", "hard_worker", "blockered_worker", "easy_close_worker",
          "reviewer", "audit", "stuck_math_audit")
_PREPARATION_ROLES = ("worker", "reviewer", "audit", "planner", "critic", "route_audit", "performance")
_RESOLVER_ENV = ("PATH", "HOME", "ELAN_HOME", "TRELLIS_ELAN_HOME", "TRELLIS_SOURCE_PATH",
                 "TRELLIS_MANAGED_AGENT_CHECKS", "TRELLIS_COMPILER_BWRAP", "TRELLIS_COMPILER_DATA_VIEW",
                 "TRELLIS_AGENT_COMPILER_CAPABILITY", "TRELLIS_COMPILER_CAPABILITY",
                 "TRELLIS_CHECK_JOBS_SOCKET", "CODEX_HOME")


class ModelPreflightError(ValueError):
    """Selected launch options cannot be admitted by the runtime catalog."""


def _digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"),
                                     ensure_ascii=False).encode()).hexdigest()


def _file_digest(path):
    with Path(path).open("rb") as stream:
        before = os.fstat(stream.fileno())
        result = hashlib.file_digest(stream, "sha256").hexdigest()
        after = os.fstat(stream.fileno())
    if (before.st_ino, before.st_size, before.st_mtime_ns) != (after.st_ino, after.st_size, after.st_mtime_ns):
        raise ModelPreflightError("Codex executable changed while its identity was read")
    return result


def native_executable_sha256(path):
    """Read one admitted native file without blocking on FIFOs or following a replacement link."""
    descriptor = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(descriptor, "rb") as stream:
        before = os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_size > MAX_EXECUTABLE_BYTES:
            raise ModelPreflightError("Codex executable must be a regular file within the 512MiB identity limit")
        if stream.read(4) != b"\x7fELF":
            raise ModelPreflightError("Codex launcher is indirect; select a concrete native ELF executable for exact launch identity")
        stream.seek(0)
        remaining = before.st_size
        digest = hashlib.sha256()
        while remaining:
            block = stream.read(min(65536, remaining))
            if not block:
                raise ModelPreflightError("Codex executable changed while its identity was read")
            digest.update(block)
            remaining -= len(block)
        after = os.fstat(stream.fileno())
        if stream.read(1) or (before.st_ino, before.st_size, before.st_mtime_ns) != (after.st_ino, after.st_size, after.st_mtime_ns):
            raise ModelPreflightError("Codex executable changed while its identity was read")
        return digest.hexdigest()


def collect_active_bindings(config):
    """Cover configured current/future lanes; disabled execution pools are inert.

    Root/phase fallbacks are already represented by their selected binding.
    Preparation fallbacks mirror modeling.driver.preparation_provider. Legacy
    scalar verification options are inert; only its actual three pools count.
    """
    result = []

    def add(role, raw, *, execution_kind="headless"):
        if not isinstance(raw, dict) or not isinstance(raw.get("provider"), str):
            raise ModelPreflightError(f"{role}: provider binding must be an object")
        fallback_models = raw.get("fallback_models") or []
        if not isinstance(fallback_models, list) or any(not isinstance(model, str) or not model for model in fallback_models):
            raise ModelPreflightError(f"{role}: fallback_models must be a list of exact model IDs")
        result.append(dict(role=role, provider=raw["provider"].strip().lower(),
                           model=raw.get("model"), effort=raw.get("effort"),
                           extra_args=copy.deepcopy(raw.get("extra_args") or []),
                           fallback_models=copy.deepcopy(fallback_models),
                           execution_kind=execution_kind))

    def block(raw, prefix=""):
        for role in _ROLES:
            if raw.get(role) is not None:
                add(prefix + role, raw[role])
        for name, binding in raw.get("stuck_math_audit_roles", {}).items():
            add(prefix + "stuck_math_audit_roles." + name, binding)
        for index, rule in enumerate(raw.get("worker_rules", [])):
            add(prefix + f"worker_rules.{index}.binding", rule["binding"])

    block(config)
    for phase, override in config.get("workflow", {}).get("phase_overrides", {}).items():
        prefix = f"workflow.phase_overrides.{phase}."
        block(override, prefix)
        for role in ("worker", "reviewer"):
            if not override.get(role) and override.get(role + "_model"):
                add(prefix + role + "_model", dict(config[role], model=override[role + "_model"]))
    preparation = config.get("preparation", {})
    for role in _PREPARATION_ROLES:
        fallback = "audit" if role in {"audit", "critic", "route_audit"} else (
            role if role in {"worker", "performance"} else "reviewer")
        binding = preparation.get(role, preparation.get(fallback, config.get("worker" if role == "worker" else "reviewer")))
        if binding is not None:
            add("preparation." + role, binding)
    for pool in ("correspondence_agents", "soundness_agents", "substantiveness_agents"):
        for index, binding in enumerate(config.get("verification", {}).get(pool, [])):
            add(f"verification.{pool}.{index}", {"provider": "claude", **binding})
    from trellis.dispatch.config import parse
    dispatch = parse(config.get("parallel_dispatch"))
    if dispatch is not None:
        for name, profile in dispatch["profiles"].items():
            add("parallel_dispatch.profiles." + name, dict(profile, effort=profile["reasoning_effort"]))
    return result


def collect_active_codex_bindings(config):
    return [binding for binding in collect_active_bindings(config) if binding["provider"] == "codex"]


def _configuration_override(value, role):
    key, separator, encoded = value.partition("=")
    if not separator or not key.strip():
        raise ModelPreflightError(f"{role}: malformed Codex configuration override")
    try:
        cursor = tomllib.loads(key.strip() + "=0")
        parts = []
        while isinstance(cursor, dict) and len(cursor) == 1:
            name, cursor = next(iter(cursor.items()))
            parts.append(name)
        if cursor != 0:
            raise ValueError("invalid key")
    except (tomllib.TOMLDecodeError, ValueError):
        raise ModelPreflightError(f"{role}: malformed Codex configuration override key") from None
    try:
        parsed_value = tomllib.loads("value=" + encoded)
        if set(parsed_value) != {"value"}:
            raise ModelPreflightError(f"{role}: ambiguous multi-assignment Codex configuration override")
        decoded = parsed_value["value"]
    except tomllib.TOMLDecodeError:
        decoded = encoded
    return parts, decoded


def catalog_arguments(binding):
    """Preserve catalog/profile configuration; refuse ambiguous exec overrides.

    The supported catalog subset is the same -c/profile subset used by
    codex_tool_mode. Unknown exec flags fail admission rather than disappear.
    Matching explicit -m/model/effort duplicates are harmless; conflicting
    duplicates would obscure which frozen selection the CLI executes.
    """
    role = binding["role"]
    model, effort = binding.get("model"), binding.get("effort")
    if any(not isinstance(value, str) or not value.strip() or value != value.strip()
           or "\x00" in value for value in (model, effort)):
        raise ModelPreflightError(f"{role}: explicit model and effort are required for catalog admission")
    args = binding.get("extra_args", [])
    if not isinstance(args, list) or any(not isinstance(arg, str) or "\x00" in arg for arg in args):
        raise ModelPreflightError(f"{role}: extra_args must be a list of NUL-free strings")
    roots, overrides = [], []
    index = 0
    while index < len(args):
        arg = args[index]
        index += 1
        name, equals, inline = arg.partition("=")
        if equals and name in {"-c", "-p"}:
            raise ModelPreflightError(f"{role}: split short Codex {name} arguments or use their long inline form")
        if name == "--ignore-user-config" and not equals:
            roots.append(arg)
            continue
        if name not in {"-c", "--config", "-p", "--profile", "--config-profile", "-m", "--model"}:
            raise ModelPreflightError(f"{role}: unsupported Codex extra_args flag {name!r}; catalog admission cannot ignore it")
        if equals:
            value = inline
        else:
            if index == len(args):
                raise ModelPreflightError(f"{role}: missing value for Codex {name}")
            value = args[index]
            index += 1
        if not value:
            raise ModelPreflightError(f"{role}: empty value for Codex {name}")
        if name in {"-m", "--model"}:
            if value != model:
                raise ModelPreflightError(f"{role}: extra_args changes the selected model")
        elif name in {"-p", "--profile", "--config-profile"}:
            roots.extend([name, value])
        else:
            parts, decoded = _configuration_override(value, role)
            key = parts[0]
            if key in {"model", "model_reasoning_effort"} and len(parts) != 1:
                raise ModelPreflightError(f"{role}: ambiguous selected model/effort configuration override")
            if key in {"model", "model_reasoning_effort"}:
                if decoded != (model if key == "model" else effort):
                    raise ModelPreflightError(f"{role}: extra_args changes the selected {key}")
            overrides.extend(["-c", value])
    return roots, overrides


def _environment(environment):
    env = dict(os.environ if environment is None else environment)
    if any(env.get(key) != os.environ.get(key) for key in _RESOLVER_ENV):
        raise ModelPreflightError("install the restored launch environment before resolving Codex runtime paths")
    return env


def codex_configuration_identity(binding, *, work_dir, burst_home, environment=None):
    """Bind known config/profile/catalog inputs without exporting their values.

    Fresh private homes and workspaces use logical keys. Existing ancestor
    configs retain their absolute identity; absent ancestor counts do not
    affect equivalence. Codex resolves precedence. We conservatively bind
    every admitted layer, including shadowed inputs, and refuse relative
    catalog indirections whose CLI-version-specific resolution is ambiguous.
    """
    env = _environment(environment)
    kind = binding.get("execution_kind", "headless")
    roots, overrides = catalog_arguments(binding)
    work = Path(work_dir).resolve()
    provider_home = (Path(env["CODEX_HOME"]).resolve() if env.get("CODEX_HOME") and kind == "headless"
                     else Path(burst_home).resolve() / ".codex")
    sources, files, documents = {}, {}, {}

    def read_config(key, path, *, include_absence=False):
        if not path.is_file():
            if include_absence:
                sources[key] = None
            return
        if path.stat().st_size > MAX_CONFIGURATION_BYTES:
            raise ModelPreflightError("Codex configuration input exceeds its size limit")
        raw = path.read_bytes()
        try:
            document = tomllib.loads(raw.decode("utf-8"))
        except (UnicodeError, tomllib.TOMLDecodeError):
            raise ModelPreflightError("malformed Codex configuration input: " + key) from None
        sources[key] = hashlib.sha256(raw).hexdigest()
        files[key] = dict(path=str(path), sha256=sources[key])
        documents[key] = document

    read_config("provider/config.toml", provider_home / "config.toml", include_absence=True)
    read_config("project/.codex/config.toml", work / ".codex/config.toml", include_absence=True)
    for parent in (() if kind == "engineering" else work.parents):
        path = parent / ".codex/config.toml"
        if path.is_file() and path != provider_home / "config.toml":
            read_config("ancestor:" + str(path), path)
    profiles = []
    index = 0
    while index < len(roots):
        if roots[index] == "--ignore-user-config":
            index += 1
        else:
            profiles.append(roots[index + 1]); index += 2
    for document in documents.values():
        if document.get("profile") is not None:
            profiles.append(document["profile"])
    if any(not isinstance(profile, str) for profile in profiles):
        raise ModelPreflightError("unsupported Codex profile filename")
    for profile in dict.fromkeys(profiles):
        if not isinstance(profile, str) or not profile or profile in {".", ".."} or Path(profile).name != profile or "\x00" in profile:
            raise ModelPreflightError("unsupported Codex profile filename")
        read_config("provider/profile:" + profile, provider_home / (profile + ".config.toml"), include_absence=True)
    if any(document.get("profile") is not None and document["profile"] not in profiles for document in documents.values()):
        raise ModelPreflightError("nested Codex profile indirection is unsupported by launch preflight")
    catalogs = []

    def referenced_catalogs(document):
        if "model_catalog_json" in document:
            catalogs.append(document["model_catalog_json"])
        configured = document.get("profiles", {})
        if isinstance(configured, dict):
            for name in profiles:
                nested = configured.get(name)
                if isinstance(nested, dict) and "model_catalog_json" in nested:
                    catalogs.append(nested["model_catalog_json"])

    for document in documents.values():
        referenced_catalogs(document)
    for index in range(1, len(overrides), 2):
        parts, value = _configuration_override(overrides[index], binding["role"])
        if parts == ["profile"]:
            raise ModelPreflightError("select a Codex profile through --profile for exact catalog configuration binding")
        document = value
        for name in reversed(parts):
            document = {name: document}
        referenced_catalogs(document)
    for catalog in catalogs:
        if not isinstance(catalog, str) or not Path(catalog).is_absolute() or "\x00" in catalog or ".." in Path(catalog).parts:
            raise ModelPreflightError("model_catalog_json requires an explicit absolute file path for launch preflight")
        path = Path(catalog)
        if not path.is_file() or path.stat().st_size > MAX_CONFIGURATION_BYTES:
            raise ModelPreflightError("Codex referenced catalog is missing or exceeds its size limit")
        key = "catalog:" + str(path)
        sources[key] = _file_digest(path)
        files[key] = dict(path=str(path), sha256=sources[key])
    return dict(sha256=_digest(sources), sources=sources, files=files)


def codex_runtime_identity(work_dir, burst_home, *, environment=None, execution_kind="headless"):
    """Resolve the executable under the actual role startup PATH, then hash it.

    Native ELF installations (including symlinks to them) are supported.
    Indirect shell/npm launchers need an explicit resolved-binary integration;
    their wrapper hash alone cannot establish the executed binary identity.
    """
    env = _environment(environment)
    work_dir, burst_home = Path(work_dir).resolve(), Path(burst_home).resolve()
    if execution_kind not in {"headless", "engineering"}:
        raise ModelPreflightError("unknown Codex execution context")
    if execution_kind == "engineering":
        from trellis.agents.engineering_context import runtime
        path, _ = runtime(env)
        executable = shutil.which("codex", path=path)
        resolved = Path(executable).resolve(strict=True)
        return dict(effective_path=path, executable=executable, resolved_executable=str(resolved),
                    executable_sha256=native_executable_sha256(resolved), format="elf")
    managed = bool(env.get("TRELLIS_MANAGED_AGENT_CHECKS") == "1" or
                   env.get("TRELLIS_COMPILER_DATA_VIEW") or env.get("TRELLIS_COMPILER_BWRAP"))
    path = worker_path_env(burst_home)
    if execution_kind == "headless":
        from trellis.modeling.environment import source_burst_environment
        source = source_burst_environment(work_dir, work_dir / ".trellis/scratch/model-preflight-source")
        prefix = source.get("TRELLIS_SOURCE_PATH", env.get("TRELLIS_SOURCE_PATH", ""))
        if prefix:
            path = prefix + os.pathsep + path
    if env.get("TRELLIS_CHECK_JOBS_SOCKET") or managed or env.get("TRELLIS_COMPILER_CAPABILITY") in {"managed", "none"}:
        path = str(work_dir / ".trellis/scripts/check-bin") + os.pathsep + path
    executable = shutil.which("codex", path=path)
    if not executable:
        raise ModelPreflightError("Codex is absent from the effective role PATH")
    executable = str(Path(executable).absolute())
    resolved = Path(executable).resolve(strict=True)
    return dict(effective_path=path, executable=executable, resolved_executable=str(resolved),
                executable_sha256=native_executable_sha256(resolved), format="elf")


def _bounded_process(argv, *, cwd, environment):
    """Bound both output and wall time; terminate the complete probe group."""
    from trellis.pv_execution import owned_process
    started = time.monotonic()
    output = {"stdout": bytearray(), "stderr": bytearray()}
    with owned_process(argv, cancel_grace=0, cwd=cwd, env=environment,
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
        try:
            with selectors.DefaultSelector() as selector:
                for name in output:
                    selector.register(getattr(process, name), selectors.EVENT_READ, name)
                while selector.get_map():
                    if time.monotonic() - started > PROBE_TIMEOUT_SECONDS:
                        raise ModelPreflightError("Codex catalog probe exceeded its 30-second deadline")
                    for key, _ in selector.select(.1):
                        data = os.read(key.fd, 65536)
                        if not data:
                            selector.unregister(key.fileobj)
                            continue
                        output[key.data].extend(data)
                        if sum(map(len, output.values())) > MAX_OUTPUT_BYTES:
                            raise ModelPreflightError("Codex catalog probe exceeded its output limit")
            returncode = process.wait(timeout=max(.01, PROBE_TIMEOUT_SECONDS - (time.monotonic() - started)))
        finally:
            # Also reap descendants that closed both captured descriptors early.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
            process.stdout.close()
            process.stderr.close()
    return dict(returncode=returncode, **{name: bytes(value).decode("utf-8", errors="strict") for name, value in output.items()})


def _probe_transport(request):
    from trellis.config import SandboxConfig
    from trellis.sandbox import wrap_command
    sandbox = request.get("sandbox") or SandboxConfig(enabled=True, backend="bwrap")
    if not sandbox.enabled or sandbox.backend != "bwrap":
        raise ModelPreflightError("Codex catalog preflight requires the production bwrap boundary")
    # The fixed inner env launcher is after bwrap's own setenv options, just
    # as the real headless startup script exports PATH after sandbox startup.
    inner = ["/usr/bin/env", "PATH=" + request["identity"]["effective_path"]]
    inner.extend(request["argv"])
    engineering_request = request.get('engineering_request') or {}
    wrapped = wrap_command(inner, sandbox=sandbox,
                           work_dir=request["work_dir"], burst_home=request["burst_home"],
                           role=("engineering" if request["execution_kind"] == "engineering" else
                                 request.get("role", "reviewer")),
                           git_preflight_path=request["identity"]["effective_path"], environment=request["environment"],
                           **({"engineering_request": {**engineering_request,
                                                        "request": engineering_request.get('request', request["argv"]),
                                                        "purpose": "provider_preflight", "sequence": None},
                               "engineering_files": request.get('engineering_files', ())}
                              if request["execution_kind"] == "engineering" else {}))
    # Use an absolute executable, and set the same PATH used by the role's
    # startup script. This also makes config-selected credential helpers see
    # the same tool search path. No shell expands provider/config arguments.
    if request["execution_kind"] == "engineering":
        from trellis.agents.codex_headless import build_launcher_script
        binding_path = Path(wrapped[5])
        launcher = build_launcher_script(script_path=binding_path, launch_cmd=wrapped,
            log_dir=binding_path.parent, log_prefix=binding_path.stem)
        wrapped = ["/bin/bash", str(launcher)]
    return _bounded_process(wrapped, cwd=request["work_dir"], environment=request["environment"])


def _probe(transport, request):
    try:
        result = transport(request)
        if result["returncode"] != 0:
            raise ModelPreflightError(f"Codex catalog command failed with exit {result['returncode']}; no provider request was submitted")
        output = result["stdout"]
        if not isinstance(output, str) or len(output.encode()) > MAX_OUTPUT_BYTES:
            raise ModelPreflightError("Codex catalog probe exceeded its output limit")
        return output
    except (OSError, subprocess.TimeoutExpired, UnicodeError, KeyError, TypeError) as error:
        raise ModelPreflightError(f"Codex catalog discovery failed ({type(error).__name__})") from error


def check_engineering_features(request, transport=None):
    """Use the same bounded capability probe at launch and direct CLI dispatch."""
    from trellis.agents.engineering_context import FEATURES, validate
    validate(request["burst_home"], request=request.get('engineering_request'))
    features = _probe(transport or _probe_transport,
                      dict(request, argv=[request["identity"]["executable"], "features", "list"]))
    for name, enabled in FEATURES.items():
        rows = [line.split() for line in features.splitlines() if line.split() and line.split()[0] == name]
        if len(rows) != 1 or rows[0][-1] != str(enabled).lower():
            raise ModelPreflightError("Engineering context exclusion is unavailable: " + name)
    from trellis.agents.engineering_context import environment
    allowed = sorted(environment(request["burst_home"], request["environment"], work=request["work_dir"]))
    # Verify both readable namespace init and child after the admitted exec.
    # Report no environment values.
    script = ("import os,pathlib\nallowed=" + repr(allowed) + "\n"
              "try:\n raw=pathlib.Path('/proc/1/environ').read_bytes()\n"
              "except PermissionError:\n raw=b''\n"
              "items=[item.split(b'=',1) for item in raw.split(b'\\0') if item]\n"
              "ok=all(key in allowed for key in os.environ) and all(len(item)==2 and item[0].decode() in allowed and os.environ.get(item[0].decode())==item[1].decode() for item in items)\n"
              "print('engineering-context-clean' if ok else 'environment-filtering-required')\n")
    observed = _probe(transport or _probe_transport,
                      dict(request, argv=["/usr/bin/python3", "-I", "-S", "-c", script])).strip()
    if observed != "engineering-context-clean":
        raise ModelPreflightError("Engineering namespace init inherits environment; clean-context launch is withheld")


def preflight_models(config, *, work_dir, burst_home, environment=None, transport=None,
                     sandbox=None, role="reviewer", engineering_context=None):
    """Validate every selected Codex binding against its exact CLI catalog.

    ``transport(request)->{returncode,stdout,stderr}`` is the unit-test seam;
    production callers use the confined default.
    """
    env = _environment(environment)
    bindings = collect_active_bindings(config)
    if engineering_context is not None:
        bindings += [dict(row, execution_kind="engineering", role="engineering." + row["role"].split(".")[-1])
                     for row in bindings if row["role"] in {"preparation.worker", "preparation.reviewer",
                                                             "preparation.planner", "preparation.performance"}]
    selected = [row for row in bindings if row["provider"] == "codex"]
    if any(row["execution_kind"] == "engineering" and row["extra_args"] for row in selected):
        raise ModelPreflightError("Engineering delivery requires Codex without additional context overrides")
    receipt = dict(schema=SCHEMA, scope="advertised Codex catalog support; no inference request",
                   bindings=[], catalogs=[], runtimes={}, configuration_contexts={},
                   skipped_non_codex=[dict(role=row["role"], provider=row["provider"]) for row in bindings if row["provider"] != "codex"])
    if not selected:
        return receipt
    transport = transport or _probe_transport
    contexts = {"headless": dict(work_dir=Path(work_dir).resolve(), burst_home=Path(burst_home).resolve())}
    if engineering_context is not None:
        contexts["engineering"] = {key: Path(engineering_context[key]).resolve() for key in ("work_dir", "burst_home")}
    for kind in dict.fromkeys(row["execution_kind"] for row in selected):
        context = contexts[kind]
        identity = codex_runtime_identity(**context, environment=env, execution_kind=kind)
        request = dict(context, identity=identity, environment=env, execution_kind=kind, sandbox=sandbox, role=role)
        version = _probe(transport, dict(request, argv=[identity["executable"], "--version"])).strip()
        if not version or len(version) > 512 or "\n" in version:
            raise ModelPreflightError("invalid Codex CLI version output")
        receipt["runtimes"][kind] = dict(identity, cli_version=version)
        if kind == "engineering":
            check_engineering_features(request, transport)
    receipt["runtime"] = receipt["runtimes"].get("headless", next(iter(receipt["runtimes"].values())))
    receipt["cli_version"] = receipt["runtime"]["cli_version"]
    cache = {}
    for binding in selected:
        roots, overrides = catalog_arguments(binding)
        kind = binding["execution_kind"]
        context, identity = contexts[kind], receipt["runtimes"][kind]
        # Hash override values in retained evidence: they can contain private
        # provider configuration. They still reach the CLI byte-for-byte.
        configuration = _digest([kind, roots, overrides])
        inputs = codex_configuration_identity(binding, **context, environment=env)
        context_sha = inputs["sha256"]
        receipt["configuration_contexts"][context_sha] = inputs
        cache_key = configuration, context_sha
        if cache_key not in cache:
            argv = [identity["executable"], *roots, "debug", "models", *overrides]
            raw = _probe(transport, dict(context, identity=identity, environment=env, execution_kind=kind,
                                         sandbox=sandbox, role=role, argv=argv))
            try:
                catalog = json.loads(raw)
                models = catalog["models"]
                if not isinstance(models, list) or not models:
                    raise ValueError("empty models")
                indexed = {}
                for model in models:
                    slug = model["slug"]
                    levels = model["supported_reasoning_levels"]
                    if (not isinstance(slug, str) or not slug or slug in indexed or not isinstance(levels, list)
                            or not levels or any(not isinstance(level, dict) or not isinstance(level.get("effort"), str)
                                                or not level["effort"] for level in levels)):
                        raise ValueError("invalid model metadata")
                    indexed[slug] = dict(slug=slug, supported_reasoning_levels=[level["effort"] for level in levels],
                                         default_reasoning_level=model.get("default_reasoning_level"))
            except (ValueError, KeyError, TypeError) as error:
                raise ModelPreflightError("invalid Codex catalog model/effort metadata") from error
            record = dict(sha256=hashlib.sha256(raw.encode()).hexdigest(), configuration_sha256=configuration,
                          context_sha256=context_sha, execution_kind=kind, model_count=len(indexed), role_bindings=[], models=[])
            receipt["catalogs"].append(record)
            cache[cache_key] = indexed, record
        indexed, record = cache[cache_key]
        models = [binding["model"], *binding["fallback_models"]]
        if any(not isinstance(model, str) or not model for model in models):
            raise ModelPreflightError(f"{binding['role']}: fallback_models must contain exact model IDs")
        admitted, fallback_support = [], []
        for position, model in enumerate(models):
            if model not in indexed:
                if position:
                    fallback_support.append(dict(model=model, advertised=False, reason="model absent from catalog"))
                    continue
                raise ModelPreflightError(f"{binding['role']}: model {model!r} is absent from {identity['cli_version']} catalog ({identity['resolved_executable']})")
            if binding["effort"] not in indexed[model]["supported_reasoning_levels"]:
                if position:
                    fallback_support.append(dict(model=model, advertised=False, reason="effort absent from catalog"))
                    continue
                raise ModelPreflightError(f"{binding['role']}: model {model!r} does not advertise effort {binding['effort']!r}")
            admitted.append(model)
            if position:
                fallback_support.append(dict(model=model, advertised=True))
            if indexed[model] not in record["models"]:
                record["models"].append(indexed[model])
        record["role_bindings"].append(binding["role"])
        receipt["bindings"].append({key: copy.deepcopy(binding[key]) for key in
                                    ("role", "provider", "model", "effort", "fallback_models", "execution_kind")}
                                  | {"configuration_sha256": configuration, "context_sha256": context_sha,
                                     "catalog_sha256": record["sha256"], "admitted_models": admitted,
                                     "fallback_catalog_support": fallback_support})
    for kind, context in contexts.items():
        current = codex_runtime_identity(**context, environment=env, execution_kind=kind)
        if any(current[key] != receipt["runtimes"][kind][key] for key in current):
            raise ModelPreflightError("Codex executable or effective PATH changed during catalog preflight")
    for binding, admitted in zip(selected, receipt["bindings"], strict=True):
        if codex_configuration_identity(binding, **contexts[binding["execution_kind"]], environment=env)["sha256"] != admitted["context_sha256"]:
            raise ModelPreflightError("Codex configuration changed during catalog preflight")
    return receipt


def validate_preflight_configuration(receipt, binding, *, work_dir, burst_home, environment=None, execution_kind="headless"):
    """Require the actual burst's provider/profile/catalog bytes to be admitted."""
    if receipt.get("schema") != SCHEMA:
        raise ModelPreflightError("unsupported Codex preflight receipt")
    binding = dict(binding, execution_kind=execution_kind)
    roots, overrides = catalog_arguments(binding)
    configuration = _digest([execution_kind, roots, overrides])
    actual = codex_configuration_identity(binding, work_dir=work_dir, burst_home=burst_home, environment=environment)
    admitted = any(row.get("execution_kind") == execution_kind
                   and row.get("configuration_sha256") == configuration
                   and row.get("context_sha256") == actual["sha256"]
                   and binding["model"] in row.get("admitted_models", [row["model"]])
                   and binding["effort"] == row["effort"] for row in receipt.get("bindings", []))
    if not admitted:
        raise ModelPreflightError("Codex provider/profile/catalog configuration differs from the admitted launch context")
    return actual


def validate_preflight_identity(receipt, *, work_dir, burst_home, environment=None, execution_kind="headless"):
    """Check a later burst resolves the admitted binary, returning its PATH.

    Per-burst shim/source/home paths may differ. Resolving them again prevents
    such differences from introducing another executable unnoticed.
    """
    if receipt.get("schema") != SCHEMA:
        raise ModelPreflightError("unsupported Codex preflight receipt")
    expected = receipt.get("runtimes", {}).get(execution_kind)
    if expected is None:
        raise ModelPreflightError("Codex execution context has no launch preflight")
    current = codex_runtime_identity(work_dir, burst_home, environment=environment, execution_kind=execution_kind)
    if any(current[key] != expected[key] for key in ("resolved_executable", "executable_sha256", "format")):
        raise ModelPreflightError("Codex runtime executable differs from the admitted launch identity")
    return current


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--burst-home", type=Path, required=True)
    parser.add_argument("--engineering-work-dir", type=Path)
    parser.add_argument("--engineering-burst-home", type=Path)
    args = parser.parse_args(argv)
    try:
        config = json.loads(args.config.read_text())
        engineering = None
        if args.engineering_work_dir or args.engineering_burst_home:
            if not args.engineering_work_dir or not args.engineering_burst_home:
                raise ModelPreflightError("both engineering context paths are required")
            engineering = dict(work_dir=args.engineering_work_dir, burst_home=args.engineering_burst_home)
        from trellis.config import SandboxConfig
        raw_sandbox = config.get("sandbox", {})
        sandbox = SandboxConfig(enabled=raw_sandbox.get("enabled", True), backend=raw_sandbox.get("backend", "bwrap"))
        receipt = preflight_models(config, work_dir=args.work_dir, burst_home=args.burst_home,
                                   sandbox=sandbox, engineering_context=engineering)
        print(json.dumps(receipt, sort_keys=True, ensure_ascii=False))
        return 0
    except (ValueError, RuntimeError, OSError) as error:
        print("Codex model preflight refused: " + str(error)[:2000], file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
