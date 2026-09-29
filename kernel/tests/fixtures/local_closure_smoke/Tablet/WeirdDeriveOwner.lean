import Tablet.Preamble
import WeirdDeriveSupport

-- [TABLET NODE: WeirdDeriveOwner]
inductive WeirdDeriveOwner where
-- BODY
  | mk : WeirdDeriveOwner
  deriving WeirdDerive
