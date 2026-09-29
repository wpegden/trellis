## When to attach `paper_focus_ranges`

Use `paper_focus_ranges` when the next worker should concentrate on a specific manuscript passage, target statement, or local argument slice rather than scanning the whole paper again.

Keep the ranges as narrow as they can be while still containing the relevant source argument. A short, well-chosen span with a reason is usually more useful than a broad chapter-sized range.

Cite only ranges you have **just read** in this review. The bridge extracts the exact lines you cite from the configured paper source and inlines them into the next worker's prompt — citing a range you have not actually consulted hands the worker source text you haven't validated and undermines the grounding loop. Do not inherit ranges from prior context without re-reading them.
