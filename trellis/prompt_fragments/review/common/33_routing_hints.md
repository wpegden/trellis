You may optionally add structured routing hints for the next worker in addition to ordinary `comments`.

- `next_worker_context_mode`: `resume` or `fresh`
- `paper_focus_ranges`: `[{start_line, end_line, reason}]`
- `work_style_hint`: `none` or `restructure`

These fields are advisory only. They help the next worker orient itself, but they do not override the kernel-authored request, checker, or authorized edit region.

When `Continue` is the chosen decision (typical), the hints above sharpen the next worker's focus. On any other decision, leave them at their empty/default values.
