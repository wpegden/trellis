import Tablet.Preamble
import WeirdDeriveSupport

-- [TABLET NODE: RenamedDeriveOwner]
inductive RenamedDeriveOwner where
-- BODY
  | mk : RenamedDeriveOwner
  deriving WeirdDeriveRenamed
