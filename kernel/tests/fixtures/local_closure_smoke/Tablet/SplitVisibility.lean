module

public import Init

public section

theorem SplitVisibilityPublic : True := by
  trivial

end

theorem SplitVisibility : True := by
  exact SplitVisibilityPublic
