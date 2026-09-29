import Tablet.Preamble

theorem CollisionRightOnly : True := by trivial

theorem SharedCollision : True := by
  exact CollisionRightOnly
