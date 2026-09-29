#!/usr/bin/env bash
# Run as root on your viewer host to put an nginx site in front of the
# trellis viewer.
#
# After this script:
#   - A new nginx site at /etc/nginx/sites-enabled/trellis routes
#     <scheme>://<host>/trellis/  ->  127.0.0.1:3301 + static at $STATIC_OUT
#   - The viewer process must be running on $VIEWER_PORT (see INSTALLATION.md
#     section 2d for the tmux launch incantation under the viewer service user)
#
# THE SITE LISTENS ON LOOPBACK BY DEFAULT. The viewer has no read
# authentication at any layer: whoever reaches it reads every run's paper,
# chat transcripts and protocol state. The control token guards writes only,
# and it rides in the URL, so on plain HTTP it is also on the wire. Reach a
# loopback site the way SECURITY.md recommends:
#
#   ssh -L 8080:127.0.0.1:80 <host>   # then http://127.0.0.1:8080/trellis/
#
# To publish it instead, name the address AND guard the read surface:
#
#   LISTEN_ADDR=0.0.0.0 AUTH_FILE=/etc/nginx/trellis.htpasswd  sudo -E $0
#   LISTEN_ADDR=0.0.0.0 ALLOW_FROM="203.0.113.4 198.51.100.0/24" sudo -E $0
#
# Publishing with neither guard is refused; ACCEPT_PLAINTEXT_EXPOSURE=1
# overrides the refusal. Either way this site is plain HTTP until you run
# certbot, so run certbot before you rely on it.
#
# Override defaults via env: BASE_PATH, STATIC_OUT, VIEWER_PORT, SITE_CONF,
# LISTEN_ADDR, AUTH_FILE, ALLOW_FROM, ACCEPT_PLAINTEXT_EXPOSURE.

set -euo pipefail

if [[ "${EUID}" -ne 0 ]]; then
  echo "run as root: sudo $0" >&2
  exit 1
fi

SITE_CONF="${SITE_CONF:-/etc/nginx/sites-enabled/trellis}"
BASE_PATH="${BASE_PATH:-/trellis}"
STATIC_OUT="${STATIC_OUT:-$HOME/trellis-web}"
VIEWER_PORT="${VIEWER_PORT:-3301}"
LISTEN_ADDR="${LISTEN_ADDR:-127.0.0.1}"
AUTH_FILE="${AUTH_FILE:-}"
ALLOW_FROM="${ALLOW_FROM:-}"
ACCEPT_PLAINTEXT_EXPOSURE="${ACCEPT_PLAINTEXT_EXPOSURE:-0}"

if [[ -e "${SITE_CONF}" ]]; then
  echo "site conf already exists: ${SITE_CONF}"
  echo "review or rm before re-running"
  exit 1
fi

if ! command -v nginx >/dev/null 2>&1; then
  echo "nginx not installed; apt install nginx first" >&2
  exit 1
fi

if [[ ! -d "${STATIC_OUT}" ]]; then
  echo "warning: ${STATIC_OUT} does not exist yet"
  echo "the viewer process writes static assets here on first launch — that's fine,"
  echo "but the directory must be readable by nginx (www-data) once it appears."
fi

is_loopback() {
  case "$1" in
    127.0.0.1|127.*|localhost|::1|'[::1]') return 0 ;;
    *) return 1 ;;
  esac
}

# Which addresses this site answers on. The loopback default is v4-only on
# purpose: `listen [::1]:80` fails to bind — and so fails `nginx -t` — on a host
# with IPv6 disabled, whereas the v6 WILDCARD the public case uses binds even
# there. So browse and tunnel to 127.0.0.1 by address; `localhost` may resolve
# to ::1 and find nothing listening.
case "${LISTEN_ADDR}" in
  0.0.0.0|'*')
    LISTEN_DIRECTIVES=$'    listen 80 default_server;\n    listen [::]:80 default_server;' ;;
  *:*)
    LISTEN_DIRECTIVES="    listen [${LISTEN_ADDR}]:80 default_server;" ;;
  *)
    LISTEN_DIRECTIVES="    listen ${LISTEN_ADDR}:80 default_server;" ;;
esac

# The read surface is the part no token protects, so it is the part an operator
# who publishes this has to decide about. These directives sit at SERVER level:
# nginx inherits allow/deny and auth_basic by nesting rather than by position,
# so every location below is covered while the control deny rules, which set
# their own, keep working exactly as they did.
ACCESS_GUARD=""
if [[ -n "${ALLOW_FROM}" ]]; then
  for cidr in $(printf '%s' "${ALLOW_FROM}" | tr ',' ' '); do
    ACCESS_GUARD+="    allow ${cidr};"$'\n'
  done
  ACCESS_GUARD+="    deny all;"$'\n'
fi
if [[ -n "${AUTH_FILE}" ]]; then
  if [[ ! -f "${AUTH_FILE}" ]]; then
    echo "warning: AUTH_FILE=${AUTH_FILE} does not exist — nginx will 500 on"
    echo "every request until it does. Create it with: htpasswd -c ${AUTH_FILE} <user>"
  fi
  ACCESS_GUARD+="    auth_basic \"trellis viewer\";"$'\n'
  ACCESS_GUARD+="    auth_basic_user_file ${AUTH_FILE};"$'\n'
fi
ACCESS_GUARD="${ACCESS_GUARD%$'\n'}"

if ! is_loopback "${LISTEN_ADDR}"; then
  if [[ -z "${ACCESS_GUARD}" && "${ACCEPT_PLAINTEXT_EXPOSURE}" != "1" ]]; then
    cat >&2 <<MSG
refusing to generate an unguarded public site.

LISTEN_ADDR=${LISTEN_ADDR} publishes the viewer to the network over plain HTTP.
Nothing authenticates reads, so that hands every run's paper, chat transcripts
and protocol state to anyone who can reach this host — and the control token
travels in the URL, so on plain HTTP it is on the wire too, which turns read
exposure into control exposure.

Pick one:
  * leave LISTEN_ADDR at 127.0.0.1 and tunnel (recommended):
      ssh -L 8080:127.0.0.1:80 <host>
  * guard the read surface:
      AUTH_FILE=/etc/nginx/trellis.htpasswd
      ALLOW_FROM="203.0.113.4 198.51.100.0/24"
  * ACCEPT_PLAINTEXT_EXPOSURE=1 to proceed anyway.

Run certbot afterwards in every case. See SECURITY.md, "Deployment modes".
MSG
    exit 1
  fi
  echo "WARNING: this site answers on ${LISTEN_ADDR} over plain HTTP."
  if [[ -z "${ACCESS_GUARD}" ]]; then
    echo "WARNING: ACCEPT_PLAINTEXT_EXPOSURE=1 — the read surface is open to"
    echo "         anyone who can reach this host, and the control token is in"
    echo "         cleartext in every URL. Run certbot now."
  else
    echo "         auth_basic/allow guards the read surface, but the credentials"
    echo "         and the control token still cross in cleartext. Run certbot now."
  fi
fi

cat > "${SITE_CONF}" <<EOF
server {
${LISTEN_DIRECTIVES}
    server_name _;
${ACCESS_GUARD}

    location = ${BASE_PATH} {
        return 301 ${BASE_PATH}/;
    }

    # The CONTROL-TOKEN SUBTREE goes to the viewer, HTML included.
    #
    # Controls live under a secret 8-character path segment:
    #   ${BASE_PATH}/<token>/           landing, controls on
    #   ${BASE_PATH}/<token>/<project>  a run, controls on
    # The viewer injects the token into the page it serves, and the page then
    # sends it back in the X-Trellis-Control header on every write. So this
    # subtree must be PROXIED rather than served from the static export —
    # ${STATIC_OUT} holds a symlink to the shipped HTML, which has no token in
    # it and never will.
    #
    # This rule is deliberately ahead of the deny rules below, so a request
    # that carries the token reaches the control plane and a request that does
    # not is refused. Caveat, stated rather than hidden: nginx cannot check
    # the token, only its shape, so a PROJECT whose slug happens to be 8
    # alphanumerics also lands here. The viewer still demands the real token
    # in the header, which is what actually authorizes a write.
    location ~* "^${BASE_PATH}/[A-Za-z0-9]{8}(/|\$)" {
        proxy_pass http://127.0.0.1:${VIEWER_PORT};
        proxy_http_version 1.1;
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$scheme;
        add_header Cache-Control "no-cache";
    }

    # The CONTROL PLANE is otherwise excluded from this public exposure.
    #
    # Everything under /api/control/ changes state: it pauses and relaunches
    # supervisors and writes operator feedback into a live run. The viewer
    # process runs un-sandboxed as the operator, so these must not be
    # reachable from whatever this server is exposed to. The viewer also
    # requires a secret header on them, and never hands that secret to a
    # proxied request — this rule is the structural layer under that, so a
    # leaked token still buys nothing here.
    #
    # If you WANT remote control, do not delete this rule: reach the viewer
    # over an SSH tunnel to 127.0.0.1 instead, which keeps the token path
    # intact. And run the viewer with TRELLIS_VIEWER_CONTROL=0 if this host
    # is publicly reachable at all. See SECURITY.md.
    # \`~*\` — CASE-INSENSITIVE, and not optional. Express matches routes
    # case-insensitively by default, so \`POST /api/CONTROL/pause/resume\`
    # reached the real handler while missing a case-sensitive deny rule. The
    # viewer now also sets \`case sensitive routing\`, so this is spelled
    # around at neither layer.
    # `uploads|create-` covers the run-creation family's pre-namespace
    # aliases (uploads, create-jobs, create-jobs.json, create-status/...) —
    # reads included, since a create job's status carries its log and the
    # whole surface is token-gated as one block. `loogle-` is the wizard's
    # host-local Loogle probe, same door.
    location ~* ^${BASE_PATH}(?:/[^/]+)?/api/(control/|pause/|external-codex/toggle|uploads|create-|loogle-) {
        deny all;
    }

    # The pre-namespace aliases of the same endpoints, kept for one release.
    # GET /api/feedback is a read-only status endpoint, so this denies by
    # method rather than by path. \`/?\` because Express also routes
    # \`/api/feedback/\`, which a \$-anchored pattern would have missed.
    location ~* ^${BASE_PATH}(?:/[^/]+)?/api/feedback/?\$ {
        limit_except GET HEAD { deny all; }
        proxy_pass http://127.0.0.1:${VIEWER_PORT};
        proxy_http_version 1.1;
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$scheme;
        add_header Cache-Control "no-cache";
    }

    # API and the proxied per-project /api routes go to the viewer process.
    location ~ ^${BASE_PATH}(?:/[^/]+)?/api/ {
        proxy_pass http://127.0.0.1:${VIEWER_PORT};
        proxy_http_version 1.1;
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$scheme;
        add_header Cache-Control "no-cache";
    }

    # Static assets served from STATIC_OUT — written by the viewer on launch.
    location ${BASE_PATH}/ {
        alias ${STATIC_OUT}/;
        index index.html;
        try_files \$uri \$uri/ ${BASE_PATH}/index.html;
        add_header Cache-Control "no-cache";
    }
}
EOF

# Drop the default-site catchall if it conflicts with our default_server. It
# only conflicts when we take the same address:port, which a loopback site does
# not — leave the host's default site alone in that case.
if ! is_loopback "${LISTEN_ADDR}" && [[ -L /etc/nginx/sites-enabled/default ]]; then
  echo "removing /etc/nginx/sites-enabled/default (conflicts with default_server)"
  rm /etc/nginx/sites-enabled/default
fi

nginx -t

# If nginx isn't already running, start + enable. Otherwise reload to pick
# up the new site config without dropping connections.
if systemctl is-active --quiet nginx; then
  systemctl reload nginx
  echo "nginx reloaded"
else
  systemctl enable --now nginx
  echo "nginx started + enabled at boot"
fi

echo
echo "ok. viewer routes (listening on ${LISTEN_ADDR}:80):"
echo "  http://<host>${BASE_PATH}/                  -> static at ${STATIC_OUT}"
echo "  http://<host>${BASE_PATH}/<project>/api/... -> proxied to :${VIEWER_PORT}"
echo
if is_loopback "${LISTEN_ADDR}"; then
  echo "this site answers on loopback only. Reach it from another machine with:"
  echo "  ssh -L 8080:127.0.0.1:80 <host>   # then http://127.0.0.1:8080${BASE_PATH}/"
  echo "(plain -L, never -L '*:8080' or -g, which republishes it on your own LAN)"
  echo "To publish it instead, see the header of this script and SECURITY.md."
else
  echo "for HTTPS: install certbot then run 'certbot --nginx -d <host>'"
  echo "until then the control token is in cleartext in every URL."
fi
