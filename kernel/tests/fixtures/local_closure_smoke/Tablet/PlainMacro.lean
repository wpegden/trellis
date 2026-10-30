-- [TABLET NODE: PlainMacro]
import Tablet.Preamble

/-- FIX 3: a node with a plain `macro` command that does NOT obviously emit a
    reserved-shaped name (it expands to a harmless term). The ban is on the
    command FAMILY, not on what the macro happens to emit, so this node must
    still be rejected: the worker could change the expansion at any time to
    synthesize a reserved-shaped declaration, and the scan cannot reason
    about every possible expansion. -/
theorem PlainMacro : True := trivial

-- A term-level macro. Harmless expansion, but the command family is banned.
macro "myTriv" : term => `((trivial : True))
