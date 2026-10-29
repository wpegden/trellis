-- [TABLET NODE: NotationLib]
import Tablet.Preamble

-- A local module that declares custom *notation* and a custom term `syntax`
-- (with a `macro_rules` elaborator). A consumer that imports this module can
-- use `myparens%` / `⟪ … ⟫` in a declaration's type or body. With an
-- `Init`-only parse (the scan-only environment), neither token is in the
-- table, so a parse over a consumer's source errors on any command using
-- them. This fixture exists to probe whether such an error blocks harvesting
-- a LATER forged declId in the same file.

-- Postfix notation usable in a type/term: `n myparens%` is `(n)`.
notation:max n "myparens%" => n

-- A custom term `syntax` + `macro_rules` so a consumer body can write `⟪ e ⟫`.
syntax (name := wrapStx) "⟪" term "⟫" : term

macro_rules
  | `(⟪ $e ⟫) => `($e)
