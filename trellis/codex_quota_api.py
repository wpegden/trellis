"""Read-only ChatGPT quota transport. Never refresh or persist credentials.

The endpoint and response fields are those used by Codex's backend client.
Unsupported auth/configuration fails closed instead of selecting another
account or launching a state-writing CLI as a fallback.
"""
from __future__ import annotations

import base64
import json
import os
from pathlib import Path
import urllib.request


_USAGE_URL = "https://chatgpt.com/backend-api/wham/usage"


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        # Do not forward an account bearer token to a redirect target.
        return None


def read_codex_usage(*, timeout_seconds: float):
    import tomllib

    if any(os.environ.get(key) for key in (
        "OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN",
    )):
        raise ValueError("environment-selected auth is unsupported")
    home = Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex")
    config_path = home / "config.toml"
    config = tomllib.loads(config_path.read_text()) if config_path.exists() else {}
    if (config.get("cli_auth_credentials_store", "file") != "file"
            or config.get("model_provider", "openai") != "openai"
            or config.get("chatgpt_base_url", "https://chatgpt.com/backend-api").rstrip("/")
            != "https://chatgpt.com/backend-api"
            or config.get("forced_login_method", "chatgpt") != "chatgpt"):
        raise ValueError("unsupported quota authentication configuration")

    # Read afresh on every probe. Never seed another writable auth store.
    auth = json.loads((home / "auth.json").read_text())
    if auth.get("auth_mode", "chatgpt") != "chatgpt" or auth.get("OPENAI_API_KEY"):
        raise ValueError("ChatGPT file auth required")
    tokens = auth["tokens"]
    access = tokens["access_token"]
    account = tokens["account_id"]
    if not isinstance(access, str) or not access or not isinstance(account, str) or not account:
        raise ValueError("missing account access token")
    forced = config.get("forced_chatgpt_workspace_id")
    if forced and account not in (forced if isinstance(forced, list) else [forced]):
        raise ValueError("account does not match configured workspace")
    # Email is display metadata only, never used to authorize the request.
    email = None
    try:
        encoded = tokens["id_token"].split(".")[1]
        claims = json.loads(base64.urlsafe_b64decode(encoded + "=" * (-len(encoded) % 4)))
        if isinstance(claims.get("email"), str):
            email = claims["email"]
    except (KeyError, IndexError, ValueError, TypeError):
        pass
    del auth, tokens

    request = urllib.request.Request(_USAGE_URL, headers={
        "Authorization": f"Bearer {access}", "ChatGPT-Account-Id": account,
        "Accept": "application/json", "User-Agent": "trellis-quota",
    })
    # HTTP errors (including expired auth) propagate to the cosmetic probe's
    # failure handler. There is no refresh, retry, login, or write-back path.
    with urllib.request.build_opener(_NoRedirect()).open(
        request, timeout=max(0.1, min(timeout_seconds, 60.0)),
    ) as response:
        payload = json.load(response)
    if not isinstance(payload, dict):
        raise ValueError("invalid usage response")
    return payload, email
