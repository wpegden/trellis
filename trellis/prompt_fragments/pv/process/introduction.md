# Trellis-PV process

Trellis-PV resolves supplied GOAL statements about a Rust program by proof or disproof. It maintains an Aeneas-derived Lean model and a dependency DAG of authored mathematical content. The source and goals remain the reference throughout the process.

Each authored node pairs Lean with natural-language content in TeX. Definition nodes give mathematical objects their meanings; proof-bearing nodes carry claims and their arguments. Imports express logical dependencies. The DAG exposes the definitions and subarguments needed to address the goals.

Work proceeds through modeling and goal stating, argument development, proof formalization, and cleanup.
