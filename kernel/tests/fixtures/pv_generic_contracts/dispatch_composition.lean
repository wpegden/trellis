theorem dispatch_composition :
  ∀ (s : Slice Byte) (bs : List Byte),
    SliceBytes s bs →
    ComputeSpec s bs ∧ ComputeInvariant s bs := by
