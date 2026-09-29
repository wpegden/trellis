This is a proof-formalization task in `local` scope.

Stay focused on the active node. `local` authorizes edits to the active node's Isar proof body and imports, plus new helper Tablet nodes that are imported into the active node's support cone. It does not authorize edits to the active node's `.tex`, changes to the active node's statement, edits to any existing other node, or repair of blockers on other nodes.

Every helper is a separate Tablet node: a new `.thy` file with exactly one principal command matching its node name, plus a matching `.tex` file. Do not add extra principal commands inside the active node's own `.thy` file.

If the honest fix requires editing another existing node, changing the active node's statement, or editing the active node's `.tex`, return `needs_restructure` and explain the broader scope needed in `comments`.
