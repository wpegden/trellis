import Tablet.Preamble

theorem CollisionLeftOnly : True := by trivial

theorem SharedCollision : True := by
  exact CollisionLeftOnly
