"""Agent communication backends.

Each backend handles one way of talking to an agent CLI.
All backends implement the same interface: send a prompt, get a result.

Backends:
- codex_headless: Script-based `codex exec` for the Codex provider.
- tmux_backend: tmux-driven interactive driver for Claude and Gemini.
  Owns session naming, prompt delivery, idle/stable detection, fallback
  model rotation on MODEL_CAPACITY_EXHAUSTED, and transcript scraping.
- script_headless: Generic `-p` headless mode used by fallback providers
  that don't have a dedicated backend.

The supervisor doesn't know which backend is used — it just calls
run_worker_burst() and run_reviewer_burst() from burst.py, which
dispatches based on `config.provider` (see trellis.burst).
"""
