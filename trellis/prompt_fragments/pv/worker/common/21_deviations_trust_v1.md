Changing any byte of the pinned crate copies or the extracted-model definition surface requires an authorized seam-repair deviation (rubric: `DEVIATIONS_trust_v1.md`, inlined elsewhere in this prompt). Leave the pinned surface untouched unless the model demonstrably diverges from real Rust at one of the three seams.

When the active node or a node you are editing claims an authorized deviation, read that deviation's `reference/` TeX file before working: for those nodes, the pinned surface together with the claimed repair files is the ground truth.

{{claimed_deviations_block}}

To request a repair: write a TeX-only file under `reference/` stating the seam, the change, and the citation argument; add a `deviation_requests` entry whose `seam_repair` object declares the seam class (`i`/`ii`/`iii`), the adapted paths with before/after SHA-256 digests, the citation, the affected challenge targets, and the tool-identity effect (`repin` with the rebuilt tool digest, `reattest_byte_identical`, or `unaffected` for repairs touching neither crate nor tool); and claim the deviation in `node_deviation_claims` for each affected node. On authorization the kernel writes the adaptation-ledger row, computes the meaning-change flag from definitional-closure fingerprints, and clears any pending model-repair marker the repair's affected targets cover.

To retire a repair that is no longer needed: `rm` the `reference/<path>.tex` file, clear every node's claim on it, and list the id in `deviation_deletions` in the same response.
