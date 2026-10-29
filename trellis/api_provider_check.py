"""Small opt-in, paid API/tool smoke test in a new sandboxed scratch repository."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import secrets
import subprocess

from trellis.adapters import ProviderConfig
from trellis.agents.api_backend import PROVIDERS, run
from trellis.config import SandboxConfig


def events_from(text):
    for line in text.splitlines():
        try:
            value = json.loads(line)
        except ValueError:
            continue  # The owned clock also records its start marker.
        if isinstance(value, dict):
            yield value


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("provider", choices=PROVIDERS)
    parser.add_argument("--output-dir", type=Path, required=True, help="New directory; must not already exist")
    parser.add_argument("--model")
    parser.add_argument("--base-url")
    parser.add_argument("--effort")
    parser.add_argument("--thinking", choices=("enabled", "disabled"))
    parser.add_argument("--resume", action="store_true", help="Also verify an exact-context continuation under the submission clock")
    args = parser.parse_args(argv)
    root = args.output_dir.expanduser().resolve()
    root.mkdir(parents=True, exist_ok=False)
    repo = root / "repo"
    repo.mkdir()
    subprocess.run(["git", "init", "-q", str(repo)], check=True)
    (repo / "Tablet").mkdir()
    home = root / "home"
    home.mkdir()
    nonce = secrets.token_hex(16)
    source = (nonce + "\n").encode()
    (repo / "input.txt").write_bytes(source)
    extra = ["--max-turns", "6", "--max-tokens", "4096", "--request-timeout", "120", "--tool-timeout", "30"]
    if args.base_url:
        extra += ["--base-url", args.base_url]
    if args.thinking:
        extra += ["--thinking", args.thinking]
    config = ProviderConfig(args.provider, args.model or PROVIDERS[args.provider][1], args.effort or 'max', extra_args=extra)
    strict = dict(require_turn_completion=True, session_record=root / 'session.json',
                  execution_record=root / 'first-clock') if args.resume else {}
    result = run(
        config,
        "Read input.txt using the shell tool. In a subsequent shell call, write Tablet/receipt.json "
        'containing {"nonce": <the stripped contents of input.txt>, "sha256": <SHA256 of input.txt bytes>}. '
        "Then read back the receipt using the shell tool and report its nonce in your final answer. "
        "Do not inspect other files or perform other work.",
        session_name="api-provider-check", work_dir=repo, burst_home=home,
        sandbox=SandboxConfig(), burst_timeout=240, startup_timeout=120, **strict,
    )
    receipt_path = repo / "Tablet/receipt.json"
    try:
        receipt = json.loads(receipt_path.read_text())
    except (OSError, ValueError):
        receipt = None
    valid = receipt == {"nonce": nonce, "sha256": hashlib.sha256(source).hexdigest()}
    final_texts = [event["item"].get("text", "") for event in events_from(result.captured_output)
                   if event.get("type") == "item.completed" and event.get("item", {}).get("type") == "agent_message"]
    ok = result.ok and result.provider_turn_completed and valid and any(nonce in text for text in final_texts)
    summary = {"ok": ok, "provider": args.provider, "model": result.usage["model"],
               "artifact_verified": valid, "turn_completed": result.provider_turn_completed,
               "usage": result.usage, "duration_seconds": round(result.duration_seconds, 2),
               "error": result.error, "transcript_path": str(result.transcript_path)}
    if args.resume and ok:
        continued = run(config,
            "This is a context-retention check. Without using any tools, repeat the nonce from the previous turn in your final answer.",
            session_name="api-provider-check", work_dir=repo, burst_home=home, sandbox=SandboxConfig(),
            burst_timeout=240, startup_timeout=120, require_turn_completion=True,
            exact_resume_thread_id=result.provider_session_id, session_record=root / 'session.json',
            execution_record=root / 'second-clock', artifact_prefix='continued')
        events = list(events_from(continued.captured_output))
        finals = [e['item'].get('text', '') for e in events if e.get('type') == 'item.completed'
                  and e.get('item', {}).get('type') == 'agent_message']
        resume_ok = (continued.ok and continued.provider_turn_completed
                     and continued.provider_session_id == result.provider_session_id
                     and any(nonce in text for text in finals)
                     and not any(e.get('type') == 'item.started' for e in events))
        summary['continuation'] = dict(ok=resume_ok, usage=continued.usage,
            same_thread=continued.provider_session_id == result.provider_session_id,
            disposition=(continued.provider_execution or {}).get('disposition'), error=continued.error)
        ok = summary['ok'] = ok and resume_ok
    (root / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
