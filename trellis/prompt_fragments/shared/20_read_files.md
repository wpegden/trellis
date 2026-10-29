Read files from disk. Do not inline whole files into your response.

Codex skill files (e.g. `lean-formalizer/SKILL.md`) are installed under `$HOME/.codex/skills/<name>/`.
In Trellis bursts, `$HOME` is the role-specific burst home, not the repository root. Do not look for skills under the repo-local `.codex` path.
