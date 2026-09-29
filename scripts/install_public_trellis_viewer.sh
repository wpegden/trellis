#!/usr/bin/env bash
# Splice the trellis viewer routes into an EXISTING, hand-managed nginx site.
#
# Unlike install_viewer_nginx.sh this script writes no `listen` directive: the
# host site decides who can reach these routes, and this script inherits that
# decision. So it checks it. The viewer authenticates no reads at all — whoever
# reaches it reads every run's paper, chat transcripts and protocol state — and
# the control token, which guards writes, rides in the URL. Splicing the viewer
# into a server block that answers on the network over plain HTTP is therefore
# refused unless you guard the read surface:
#
#   AUTH_FILE=/etc/nginx/trellis.htpasswd            sudo -E $0
#   ALLOW_FROM="203.0.113.4 198.51.100.0/24"         sudo -E $0
#
# ACCEPT_PLAINTEXT_EXPOSURE=1 overrides the refusal. Either guard is written
# into the spliced locations only, so the rest of the host site is untouched.
#
# Override defaults via env: SITE_CONF, BASE_PATH, STATIC_OUT, VIEWER_PORT,
# AUTH_FILE, ALLOW_FROM, ACCEPT_PLAINTEXT_EXPOSURE.

set -euo pipefail

SITE_CONF="${SITE_CONF:-/etc/nginx/sites-enabled/lagent-chats}"
BASE_PATH="${BASE_PATH:-/trellis}"
STATIC_OUT="${STATIC_OUT:-$HOME/trellis-web}"
VIEWER_PORT="${VIEWER_PORT:-3301}"
AUTH_FILE="${AUTH_FILE:-}"
ALLOW_FROM="${ALLOW_FROM:-}"
ACCEPT_PLAINTEXT_EXPOSURE="${ACCEPT_PLAINTEXT_EXPOSURE:-0}"

if [[ "${EUID}" -ne 0 ]]; then
  echo "run as root: sudo $0" >&2
  exit 1
fi

if [[ ! -f "${SITE_CONF}" ]]; then
  echo "nginx site config not found: ${SITE_CONF}" >&2
  exit 1
fi

if [[ -n "${AUTH_FILE}" && ! -f "${AUTH_FILE}" ]]; then
  echo "warning: AUTH_FILE=${AUTH_FILE} does not exist — nginx will 500 on every"
  echo "request until it does. Create it with: htpasswd -c ${AUTH_FILE} <user>"
fi

python3 - "${SITE_CONF}" "${BASE_PATH}" "${STATIC_OUT}" "${VIEWER_PORT}" \
         "${AUTH_FILE}" "${ALLOW_FROM}" "${ACCEPT_PLAINTEXT_EXPOSURE}" <<'PY'
from pathlib import Path
import re
import os
import sys

site_conf = Path(sys.argv[1])
base = sys.argv[2].rstrip("/")
static_out = sys.argv[3]
port = sys.argv[4]
auth_file = sys.argv[5]
allow_from = [c for c in sys.argv[6].replace(",", " ").split() if c]
accept_plaintext = sys.argv[7] == "1"

text = site_conf.read_text(encoding="utf-8")
marker = f"location = {base} {{"
if marker in text:
    print(f"{base} routes already installed in {site_conf}")
    raise SystemExit(0)

needle = os.environ.get("TRELLIS_SITE_INSERT_NEEDLE") or "\n    include /etc/nginx/snippets/trellis-source.conf;\n"
if needle not in text:
    raise SystemExit(f"could not find insertion marker in {site_conf}")


def enclosing_server_block(text, at):
    """The `server { … }` the insertion point falls inside, or None.

    Textual, because parsing nginx properly is not worth it here: take the last
    `server {` that opens before the insertion point and brace-match forward.
    """
    starts = [m.end() for m in re.finditer(r"^\s*server\s*\{", text[:at], re.M)]
    if not starts:
        return None
    start = starts[-1]
    depth = 1
    for i in range(start, len(text)):
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
            if depth == 0:
                return text[start:i] if i > at else None
    return text[start:]


def plaintext_public_listens(block):
    """listen directives that put this block on the network without TLS."""
    if block is None:
        return ["<could not locate the enclosing server block>"]
    listens = re.findall(r"^\s*listen\s+([^;]+);", block, re.M)
    if not listens:
        # No listen at all means nginx's default, which is *:80.
        return ["<none — nginx defaults to *:80>"]
    bad = []
    for directive in listens:
        value = directive.strip()
        lowered = value.lower()
        if "ssl" in lowered or "quic" in lowered:
            continue
        addr = value.split()[0]
        if addr.rsplit(":", 1)[-1] == "443":
            continue
        host = addr.rsplit(":", 1)[0] if ":" in addr else ""
        if addr.startswith("unix:") or host in ("127.0.0.1", "localhost", "[::1]"):
            continue
        if addr in ("127.0.0.1", "localhost", "[::1]"):
            continue
        bad.append(value)
    return bad


guard_lines = [f"allow {cidr};" for cidr in allow_from]
if allow_from:
    guard_lines.append("deny all;")
if auth_file:
    guard_lines.append('auth_basic "trellis viewer";')
    guard_lines.append(f"auth_basic_user_file {auth_file};")

exposed = plaintext_public_listens(enclosing_server_block(text, text.index(needle)))
if exposed and not guard_lines and not accept_plaintext:
    raise SystemExit(
        f"refusing to splice the viewer into a plaintext public server block.\n\n"
        f"{site_conf} answers on: {', '.join(exposed)}\n\n"
        "Nothing authenticates reads, so these routes would hand every run's\n"
        "paper, chat transcripts and protocol state to anyone who can reach\n"
        "this host — and the control token travels in the URL, so on plain HTTP\n"
        "it is on the wire too, which turns read exposure into control exposure.\n\n"
        "Pick one:\n"
        "  * splice into a TLS server block instead, and decide the read\n"
        "    surface there;\n"
        "  * guard the read surface (written into the spliced locations only):\n"
        "      AUTH_FILE=/etc/nginx/trellis.htpasswd\n"
        '      ALLOW_FROM="203.0.113.4 198.51.100.0/24"\n'
        "  * ACCEPT_PLAINTEXT_EXPOSURE=1 to proceed anyway — also the answer if\n"
        "    the plaintext listen only redirects to https, which this check\n"
        "    reads the listen directives to decide and so cannot see.\n\n"
        'See SECURITY.md, "Deployment modes".'
    )
if exposed:
    print(f"WARNING: {site_conf} answers on {', '.join(exposed)} — plain HTTP.")
    if guard_lines:
        print("         The guard covers the read surface, but the credentials")
        print("         and the control token still cross in cleartext.")
    else:
        print("         ACCEPT_PLAINTEXT_EXPOSURE=1 — the read surface is open,")
        print("         and the control token is in cleartext in every URL.")

# Written into each spliced location rather than at server level, so an
# allowlist for the viewer cannot lock the operator out of the rest of the site.
guard = "".join(f"        {line}\n" for line in guard_lines)

block = f"""

    location = {base} {{
        return 301 {base}/;
    }}

    # The control plane — every state-changing viewer endpoint — is excluded
    # from this public exposure outright. See the same rule and its reasoning
    # in scripts/install_viewer_nginx.sh, and SECURITY.md.
    # The control-token subtree — HTML included — goes to the viewer, so the
    # viewer can inject the token into the page it serves. Serving it from
    # {static_out} would hand back the shipped HTML, which has no token.
    # Ahead of the deny rules on purpose: with the token you reach the
    # control plane, without it you do not. nginx can only check the token's
    # SHAPE, so an 8-alphanumeric project slug also lands here; the viewer
    # still requires the real token in the X-Trellis-Control header.
    location ~* "^{base}/[A-Za-z0-9]{{8}}(/|$)" {{
{guard}        proxy_pass http://127.0.0.1:{port};
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        add_header Cache-Control "no-cache";
    }}

    # `~*` — case-insensitive, because Express matches routes that way by
    # default and `/api/CONTROL/...` would otherwise slip past.
    # `uploads|create-` covers the run-creation family's pre-namespace
    # aliases (uploads, create-jobs, create-status/...), reads included;
    # `loogle-` is the wizard's host-local Loogle probe, same door.
    location ~* ^{base}(?:/[^/]+)?/api/(control/|pause/|external-codex/toggle|uploads|create-|loogle-) {{
        deny all;
    }}

    # Pre-namespace alias of the same. GET /api/feedback is a read-only
    # status endpoint, so this denies by method rather than by path; `/?`
    # covers the trailing-slash spelling Express also routes.
    location ~* ^{base}(?:/[^/]+)?/api/feedback/?$ {{
{guard}        limit_except GET HEAD {{ deny all; }}
        proxy_pass http://127.0.0.1:{port};
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        add_header Cache-Control "no-cache";
    }}

    location ~ ^{base}(?:/[^/]+)?/api/ {{
{guard}        proxy_pass http://127.0.0.1:{port};
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        add_header Cache-Control "no-cache";
    }}

    location {base}/ {{
{guard}        alias {static_out}/;
        index index.html;
        try_files $uri $uri/ {base}/index.html;
        add_header Cache-Control "no-cache";
    }}
"""

text = text.replace(needle, block + needle, 1)
site_conf.write_text(text, encoding="utf-8")
print(f"installed {base} routes into {site_conf}")
PY

nginx -t
systemctl reload nginx
echo "nginx reloaded with ${BASE_PATH} routes -> 127.0.0.1:${VIEWER_PORT}"
