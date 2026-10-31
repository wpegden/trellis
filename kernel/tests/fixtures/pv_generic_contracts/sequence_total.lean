theorem sequence_total :
  ∀ (s : Slice Byte),
    ∃ out : Output,
      transform s = ok out := by
