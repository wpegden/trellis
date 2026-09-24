import Tablet.Preamble

-- [TABLET NODE: EnumDerivedDecEq]
inductive EnumDerivedDecEq : Type where
-- BODY
  | alpha : EnumDerivedDecEq
  | beta : EnumDerivedDecEq
  deriving DecidableEq
