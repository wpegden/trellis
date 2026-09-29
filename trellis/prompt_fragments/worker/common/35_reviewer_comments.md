## Reviewer comments

The text below is guidance forwarded by the kernel. It may come from the reviewer, or it may be automatic supervisor guidance for a deterministic cleanup task. Keep in mind, the kernel-authored request, contract, and checker still control what is legal and accepted.

If reviewer comments suggest out-of-scope edits, checker-rejected file shapes, or any other change that conflicts with the kernel-authored contract, do not force that change. Make the closest legal improvement you can. If you can name a specific broader repair that would make the intended move legal or effective, return `needs_restructure`; otherwise return `stuck`, and explain the mismatch in your normal `comments` field. Use `system_feedback` only for prompt/tool/runtime/setup debugging notes; these will be read later by the developers of this system.

{{reviewer_comments_text}}
