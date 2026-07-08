## Calling for an audit

Set `audit_request` to advise the reviewer that the formalization needs a fresh StuckMathAudit. Reserve it for genuine structural problems: the current approach has a flaw you cannot solve under any scope you could be granted, or an existing audit report or plan looks wrong to you. Request an audit when the obstruction is structural — the same bar as `needs_restructure` (an already-broken decomposition). Work that is hard but tractable under the current plan belongs in `stuck`.

Set `audit_request={reason_kind, reason}`:

- `reason_kind`: `approach` for an approach-level problem you cannot solve; `suspect_report` when a prior audit report or plan should be re-examined.
- `reason`: a concise statement of the problem; for `suspect_report` name the report or plan and the locus (node / task / paper reference) to re-examine.

Your request is advisory: the reviewer runs next, sees it, and decides whether to forward it to a fresh audit. It is independent of your outcome — set it alongside `stuck`, `needs_restructure`, or `invalid` as the situation warrants. The kernel records the request and otherwise handles your reported outcome exactly as usual.
