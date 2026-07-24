-- [TABLET NODE: CommandLib]
import Tablet.Preamble

-- A local module declaring a custom *command* macro. A consumer that
-- imports this module can write `declare_trivial foo` as a top-level
-- command, which expands to `theorem foo : True := trivial`. Under the
-- `Init`-only scan-only parse, `declare_trivial` is not a known command
-- token, so a consumer using it parse-errors on that command. This probes
-- whether the error blocks harvesting a LATER forged declId.

-- Custom top-level command: `declare_trivial NAME` expands to a theorem.
syntax (name := declTrivial) "declare_trivial " ident : command

macro_rules
  | `(declare_trivial $name:ident) => `(theorem $name : True := trivial)
