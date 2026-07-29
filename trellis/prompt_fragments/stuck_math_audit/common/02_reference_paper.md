## Reference paper

The `.tex` source of the paper being formalized is on disk at `{{paper_tex_path}}` (path relative to repo root). The repo is mounted read-only for this role except for your scratch directory, so `cat`, `rg`, `sed`, `head`, etc. work directly on the file.

The paper is the primary mathematical source for the audit. Unless the paper itself has a fundamental mathematical deficiency, its correctness is the reason formalization should be possible. When autoformalization appears to be stuck or spinning, use the paper to identify which incorrect or incomplete statements are blocking real progress, and how the formalization strategy needs to change.
