# Examples

Default artifacts Trellis ships for a first run:

- `trellis.config.json` / `trellis.policy.json` — the default run configuration
  (codex provider). `setup_repo.sh` uses these unless you override
  `CONFIG_TEMPLATE=...`.
- `connectivity_threshold_gnp.tex` — a sample paper to formalize / test against.

## Sample paper

`connectivity_threshold_gnp.tex` is a self-contained proof, written by ChatGPT,
of the classical connectivity threshold for the Erdős–Rényi random graph
G(n, p): as np − log n → −∞, c, or +∞, the probability that G(n, p) is
connected tends to 0, e^(−e^(−c)), or 1 respectively. The argument uses a
second-moment / Bonferroni analysis of isolated vertices plus a spanning-tree
counting bound that rules out other small components. Its theorem environments
are already canonical, so the paper-environment normalizer is a no-op on it.

Point setup at it with:

    ./scripts/setup_repo.sh <repo_path> examples/connectivity_threshold_gnp.tex
