import Tablet.Preamble

local syntax "policy_true" : term
local macro_rules
  | `(policy_true) => `(True)

-- [TABLET NODE: PolicyLocalSyntax]
theorem PolicyLocalSyntax : policy_true := by
-- BODY
  trivial
