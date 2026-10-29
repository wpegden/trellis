"""OAuth client credentials baked into the gemini-cli bundle.

Extracted (one-shot, by hand) from the published @google/gemini-cli npm
bundle:
    bundle/chunk-SZYCJREE.js
    var OAUTH_CLIENT_ID = "...";
    var OAUTH_CLIENT_SECRET = "...";

These are the public OAuth client credentials that the gemini-cli ships
with — they are NOT secret in any meaningful sense (the bundle is
distributed unencrypted on npm). Pinning them here lets us refresh
access tokens without spawning the CLI.

Re-extract via:
    grep -A1 "OAUTH_CLIENT_ID = " <gemini-cli-bundle-dir>/chunk-*.js
"""

from __future__ import annotations
import os

# Source: gemini-cli bundle, chunk-SZYCJREE.js (npm @google/gemini-cli).
GEMINI_OAUTH_CLIENT_ID = os.environ.get("GEMINI_OAUTH_CLIENT_ID", "")
GEMINI_OAUTH_CLIENT_SECRET = os.environ.get("GEMINI_OAUTH_CLIENT_SECRET", "")

# Token endpoint Google's OAuth servers expose for refresh-token grants.
GEMINI_OAUTH_TOKEN_ENDPOINT = "https://oauth2.googleapis.com/token"

# Code Assist API — the same endpoint the gemini-cli's `/model` slash
# command pulls quota from. Undocumented but stable since gemini-cli 0.x.
CODE_ASSIST_BASE = "https://cloudcode-pa.googleapis.com"
CODE_ASSIST_RETRIEVE_USER_QUOTA = f"{CODE_ASSIST_BASE}/v1internal:retrieveUserQuota"
CODE_ASSIST_LOAD = f"{CODE_ASSIST_BASE}/v1internal:loadCodeAssist"
