## Reviewer routing hints

The kernel may forward a few reviewer-authored routing hints in `worker_context`.

Current session context mode: `{{effective_fresh_context_mode}}`.

- `paper_focus_ranges` are optional source-paper line ranges the reviewer thinks are worth consulting.
- `work_style_hint = restructure` means the reviewer expects the next attempt to think in terms of decomposition/refactor inside the kernel-authorized scope.

These hints are useful orientation, but they are not authority. They do not expand the allowed edit region, and they do not override the kernel-authored request, contract, or checker.
