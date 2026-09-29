#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: bash .trellis/runtime/src/scripts/loogle_json.sh [--timeout SECONDS] [--raw] "query"

Query the local Loogle JSON endpoint with a generous timeout and fail clearly when the
service is just being slow.

Options:
  --timeout SECONDS   curl max-time in seconds (default: 60)
  --raw               emit validated raw JSON instead of pretty-printed JSON
EOF
}

timeout_seconds=60
raw=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --timeout)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --timeout requires an argument" >&2
        exit 2
      fi
      timeout_seconds="$2"
      shift 2
      ;;
    --raw)
      raw=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    -*)
      echo "ERROR: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
    *)
      break
      ;;
  esac
done

if [[ $# -ne 1 ]]; then
  usage >&2
  exit 2
fi

query="$1"
encoded_query="$(python3 - "$query" <<'PY'
import sys
import urllib.parse

print(urllib.parse.quote_plus(sys.argv[1]))
PY
)"

tmp="$(mktemp)"
cleanup() {
  rm -f "$tmp"
}
trap cleanup EXIT

url="http://127.0.0.1:8088/json?q=${encoded_query}"

rc=0
curl --silent --show-error --fail --max-time "$timeout_seconds" "$url" -o "$tmp" || rc=$?
if [[ "$rc" -ne 0 ]]; then
  if [[ "$rc" -eq 7 ]]; then
    # curl exit 7 = couldn't connect: no Loogle server is listening. Retrying
    # with a longer timeout will never help — say so instead of advising a wait.
    echo "Loogle is not reachable at 127.0.0.1:8088 (connection refused)." >&2
    echo "No local Loogle server is running here. Proceed without Loogle — fall" >&2
    echo "back to lake scratch checks or repository grep; retrying will not help." >&2
  else
    # Timeout (28) or other transient failure: the service may just be slow.
    echo "Loogle query failed or timed out after ${timeout_seconds}s (curl exit ${rc})." >&2
    echo "The local service is often slow on cold or broad searches." >&2
    echo "Wait longer before giving up, for example:" >&2
    echo "  bash $0 --timeout 120 \"$query\"" >&2
  fi
  exit "$rc"
fi

python3 - "$tmp" "$raw" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
raw = sys.argv[2] == "1"

text = path.read_text(encoding="utf-8", errors="replace")
if not text.strip():
    print("Loogle returned empty output.", file=sys.stderr)
    print("The local service may still be busy; try again with a longer timeout.", file=sys.stderr)
    sys.exit(1)

try:
    payload = json.loads(text)
except json.JSONDecodeError as exc:
    print(f"Loogle returned non-JSON output: {exc}", file=sys.stderr)
    snippet = text[:400].strip()
    if snippet:
        print("--- output snippet ---", file=sys.stderr)
        print(snippet, file=sys.stderr)
    sys.exit(1)

if raw:
    sys.stdout.write(text)
    if not text.endswith("\n"):
        sys.stdout.write("\n")
else:
    json.dump(payload, sys.stdout, indent=2, ensure_ascii=False)
    sys.stdout.write("\n")
PY
