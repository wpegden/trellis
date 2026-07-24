//! PV "prove OR disprove" — the DORMANT STORE.
//!
//! A `Decide` target is a pair `(T, ¬T)` where, at any time, EXACTLY ONE side
//! is LIVE and the other DORMANT. The live side is an ordinary tablet node
//! (`Tablet/<Name>.lean` + `.tex`), present in `present_nodes` and worked by
//! every lane. The dormant side must be INVISIBLE to the whole process: it is
//! not in `present_nodes`, raises no blocker, is off every lane frontier, and
//! is never byte-pinned while dormant.
//!
//! The mechanism is purely physical: `present_nodes_from_repo` scans ONLY
//! `Tablet/` (one directory, non-recursively), so a node file that is NOT in
//! `Tablet/` is automatically excluded from `present_nodes` and from every one
//! of its ~760 downstream uses. The dormant side's `.lean`/`.tex` therefore
//! live in a SEPARATE directory, `Dormant/`, beside `Tablet/` — a directory no
//! tablet scan, lake target, or closure/coverage pass ever reads. Bringing a
//! dormant node back is a single operation — a polarity flip — which moves the
//! newly-live side `Dormant/ → Tablet/` and the newly-dormant side
//! `Tablet/ → Dormant/`, atomically.
//!
//! Dormant files are NEVER deleted — a flip must always be able to resurrect
//! the other side — with ONE narrow exception: when a flip leg finds its
//! destination already occupied by a byte-identical copy of its source (a
//! dual-location duplicate, e.g. a stale seed stub), the flip removes the
//! REDUNDANT source copy; the content survives verbatim at the destination, so
//! resurrectability is intact. A dual-location duplicate with DIVERGENT content
//! makes the flip refuse loudly rather than overwrite either copy (on Linux a
//! bare `rename` would silently REPLACE the destination — e.g. clobber an
//! accepted `Tablet/` proof with a stale `Dormant/` stub). The flip is the SOLE
//! writer of `Dormant/`.

use crate::model::NodeId;
use std::fs;
use std::path::{Path, PathBuf};

/// The dormant-store directory beside `Tablet/`. No tablet scan, lake `lean_lib
/// «Tablet»` (srcDir `.`, module root `Tablet`), or closure/coverage pass ever
/// reads it: every such walk is rooted at `Tablet/`, and the byte-pinned node
/// slices only `import Tablet.*`, never `import Dormant.*`.
pub fn dormant_dir(repo_path: &Path) -> PathBuf {
    repo_path.join("Dormant")
}

fn tablet_dir(repo_path: &Path) -> PathBuf {
    repo_path.join("Tablet")
}

/// The `(.lean, .tex)` pair for a node under `Tablet/`.
fn tablet_paths(repo_path: &Path, node: &NodeId) -> (PathBuf, PathBuf) {
    let dir = tablet_dir(repo_path);
    (
        dir.join(format!("{}.lean", node.as_str())),
        dir.join(format!("{}.tex", node.as_str())),
    )
}

/// The `(.lean, .tex)` pair for a node under `Dormant/`.
fn dormant_paths(repo_path: &Path, node: &NodeId) -> (PathBuf, PathBuf) {
    let dir = dormant_dir(repo_path);
    (
        dir.join(format!("{}.lean", node.as_str())),
        dir.join(format!("{}.tex", node.as_str())),
    )
}

/// True iff `node` has a `.lean` file in `Tablet/` (i.e. it is currently live).
pub fn node_in_tablet(repo_path: &Path, node: &NodeId) -> bool {
    tablet_paths(repo_path, node).0.exists()
}

/// True iff `node` has a `.lean` file in `Dormant/` (i.e. it is currently
/// dormant on disk).
pub fn node_in_dormant(repo_path: &Path, node: &NodeId) -> bool {
    dormant_paths(repo_path, node).0.exists()
}

/// Move a node's `.lean` + `.tex` from `Dormant/` into `Tablet/`. Re-runnable
/// after a torn flip: a source file already absent is a no-op. A node present
/// in BOTH locations is NOT silently resolved by the move: an identical
/// duplicate is collapsed onto the destination (redundant source removed) and
/// a divergent duplicate is a hard error (see `move_if_present`). Creates
/// `Tablet/` if absent.
fn promote_to_tablet(repo_path: &Path, node: &NodeId) -> Result<(), String> {
    fs::create_dir_all(tablet_dir(repo_path))
        .map_err(|err| format!("dormant flip: failed to ensure Tablet/: {err}"))?;
    let (dorm_lean, dorm_tex) = dormant_paths(repo_path, node);
    let (tab_lean, tab_tex) = tablet_paths(repo_path, node);
    move_if_present(&dorm_lean, &tab_lean)?;
    move_if_present(&dorm_tex, &tab_tex)?;
    Ok(())
}

/// Move a node's `.lean` + `.tex` from `Tablet/` into `Dormant/`. Same
/// re-run/duplicate semantics as `promote_to_tablet` (see `move_if_present`).
/// Creates `Dormant/` if absent.
fn demote_to_dormant(repo_path: &Path, node: &NodeId) -> Result<(), String> {
    fs::create_dir_all(dormant_dir(repo_path))
        .map_err(|err| format!("dormant flip: failed to ensure Dormant/: {err}"))?;
    let (tab_lean, tab_tex) = tablet_paths(repo_path, node);
    let (dorm_lean, dorm_tex) = dormant_paths(repo_path, node);
    move_if_present(&tab_lean, &dorm_lean)?;
    move_if_present(&tab_tex, &dorm_tex)?;
    Ok(())
}

/// Rename `from`→`to` when `from` exists; a `from` that is already absent is a
/// no-op (idempotent re-run after a partial/crashed flip). A `rename` within
/// one filesystem is atomic, so an individual file is never torn.
///
/// OVERWRITE GUARD. On Linux `rename` atomically REPLACES an existing
/// destination, so an unguarded move would let a stale duplicate (e.g. a seed
/// stub left in `Dormant/` after the one-side invariant was violated) silently
/// destroy the destination — potentially an accepted `Tablet/` proof. When the
/// destination already exists:
///
/// - byte-identical to the source → the source is REDUNDANT; remove it and
///   keep the destination untouched (the sole exception to "dormant files are
///   never deleted": the content survives verbatim at the destination);
/// - divergent content → hard error naming both paths; NEITHER file is
///   modified. The caller (the flip) propagates this to the runtime, which
///   fails loud (`RuntimeError::InvalidRuntimeState`).
///
/// The guard cannot fire on legitimate torn-flip recovery: after a completed
/// promote leg the re-run sees the source ABSENT and no-ops above before any
/// destination check.
fn move_if_present(from: &Path, to: &Path) -> Result<(), String> {
    if !from.exists() {
        return Ok(());
    }
    if to.exists() {
        let from_bytes = fs::read(from)
            .map_err(|err| format!("dormant flip: failed to read {}: {err}", from.display()))?;
        let to_bytes = fs::read(to)
            .map_err(|err| format!("dormant flip: failed to read {}: {err}", to.display()))?;
        if from_bytes == to_bytes {
            // Identical duplicate: collapse onto the destination. Removing the
            // source (rather than renaming over the destination) keeps the
            // destination inode untouched; content is preserved verbatim.
            return fs::remove_file(from).map_err(|err| {
                format!(
                    "dormant flip: failed to remove redundant duplicate {} (identical copy \
                     kept at {}): {err}",
                    from.display(),
                    to.display()
                )
            });
        }
        return Err(format!(
            "dormant flip: refusing to move {} -> {}: dual-location duplicate with divergent \
             content; reconcile before flipping — refusing to overwrite",
            from.display(),
            to.display()
        ));
    }
    fs::rename(from, to).map_err(|err| {
        format!(
            "dormant flip: failed to move {} -> {}: {err}",
            from.display(),
            to.display()
        )
    })
}

/// Apply a polarity flip on disk for a `Decide` pair: promote the NEWLY-LIVE
/// node `Dormant/ → Tablet/` and demote the NEWLY-DORMANT node
/// `Tablet/ → Dormant/`.
///
/// CRASH-SAFETY. The flip is two atomic `rename`s, and is re-runnable: if a
/// crash interrupts it, re-invoking it completes the move (each leg is a no-op
/// when its source is already gone). The PROMOTE is done first so the invariant
/// "the live side is present in `Tablet/`" is restored before the old live side
/// leaves. This means the only reachable intermediate state has BOTH sides in
/// `Tablet/` (never NEITHER): a re-run then simply demotes the remaining old
/// side. A torn flip is therefore self-healing on the next call and never
/// leaves the pair with no node in `Tablet/`.
///
/// `newly_live`/`newly_dormant` are the NODE names (file stems) of the two
/// polarities — for a Decide pair, `<Primary>` and `<Primary>__Refutation`.
pub fn flip_decide_pair_on_disk(
    repo_path: &Path,
    newly_live: &NodeId,
    newly_dormant: &NodeId,
) -> Result<(), String> {
    promote_to_tablet(repo_path, newly_live)?;
    demote_to_dormant(repo_path, newly_dormant)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn read(path: &Path) -> Option<String> {
        fs::read_to_string(path).ok()
    }

    #[test]
    fn flip_moves_both_sides_and_is_reversible() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let primary = NodeId::from("Correct");
        let refutation = NodeId::from("Correct__Refutation");

        // Seed default layout: primary live (Tablet/), refutation dormant.
        touch(&repo.join("Tablet").join("Correct.lean"), "PRIMARY-LEAN");
        touch(&repo.join("Tablet").join("Correct.tex"), "PRIMARY-TEX");
        touch(
            &repo.join("Dormant").join("Correct__Refutation.lean"),
            "REF-LEAN",
        );
        touch(
            &repo.join("Dormant").join("Correct__Refutation.tex"),
            "REF-TEX",
        );

        // Flip to Disprove: refutation becomes live, primary dormant.
        flip_decide_pair_on_disk(repo, &refutation, &primary).unwrap();
        assert!(node_in_tablet(repo, &refutation));
        assert!(!node_in_tablet(repo, &primary));
        assert!(node_in_dormant(repo, &primary));
        assert!(!node_in_dormant(repo, &refutation));
        assert_eq!(
            read(&repo.join("Tablet").join("Correct__Refutation.lean")).as_deref(),
            Some("REF-LEAN")
        );
        assert_eq!(
            read(&repo.join("Dormant").join("Correct.tex")).as_deref(),
            Some("PRIMARY-TEX")
        );

        // Flip back: byte-identical to the original layout.
        flip_decide_pair_on_disk(repo, &primary, &refutation).unwrap();
        assert!(node_in_tablet(repo, &primary));
        assert!(node_in_dormant(repo, &refutation));
        assert_eq!(
            read(&repo.join("Tablet").join("Correct.lean")).as_deref(),
            Some("PRIMARY-LEAN")
        );
        assert_eq!(
            read(&repo.join("Dormant").join("Correct__Refutation.lean")).as_deref(),
            Some("REF-LEAN")
        );
    }

    #[test]
    fn flip_is_crash_safe_rerunnable_after_partial_promote() {
        // Simulate a crash AFTER the promote leg but BEFORE the demote leg:
        // both sides are in Tablet/, the new-live side gone from Dormant/.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let primary = NodeId::from("Correct");
        let refutation = NodeId::from("Correct__Refutation");

        touch(&repo.join("Tablet").join("Correct.lean"), "PRIMARY-LEAN");
        touch(&repo.join("Tablet").join("Correct.tex"), "PRIMARY-TEX");
        // Promoted already: refutation now in Tablet/, not Dormant/.
        touch(
            &repo.join("Tablet").join("Correct__Refutation.lean"),
            "REF-LEAN",
        );
        touch(
            &repo.join("Tablet").join("Correct__Refutation.tex"),
            "REF-TEX",
        );

        // Both sides present in Tablet/ — never NEITHER.
        assert!(node_in_tablet(repo, &primary));
        assert!(node_in_tablet(repo, &refutation));

        // Re-run the same flip (refutation→live, primary→dormant): completes.
        flip_decide_pair_on_disk(repo, &refutation, &primary).unwrap();
        assert!(node_in_tablet(repo, &refutation));
        assert!(!node_in_tablet(repo, &primary));
        assert!(node_in_dormant(repo, &primary));
    }

    #[test]
    fn flip_collapses_identical_dual_location_duplicate_onto_destination() {
        // One-side invariant violated with an IDENTICAL duplicate: the
        // promote leg's destination already holds the same bytes as its
        // source. The flip must succeed, keep the destination untouched, and
        // remove the redundant source copy.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let primary = NodeId::from("Correct");
        let refutation = NodeId::from("Correct__Refutation");

        touch(&repo.join("Tablet").join("Correct.lean"), "PRIMARY-LEAN");
        touch(&repo.join("Tablet").join("Correct.tex"), "PRIMARY-TEX");
        // Refutation present in BOTH Dormant/ (its proper home) and Tablet/
        // (stale duplicate), byte-identical.
        touch(
            &repo.join("Dormant").join("Correct__Refutation.lean"),
            "REF-LEAN",
        );
        touch(
            &repo.join("Dormant").join("Correct__Refutation.tex"),
            "REF-TEX",
        );
        touch(
            &repo.join("Tablet").join("Correct__Refutation.lean"),
            "REF-LEAN",
        );
        touch(
            &repo.join("Tablet").join("Correct__Refutation.tex"),
            "REF-TEX",
        );

        // Flip to Disprove: promote leg finds identical duplicates at the
        // destination; the flip still succeeds end to end.
        flip_decide_pair_on_disk(repo, &refutation, &primary).unwrap();
        // Destination content untouched; redundant Dormant/ copies gone.
        assert_eq!(
            read(&repo.join("Tablet").join("Correct__Refutation.lean")).as_deref(),
            Some("REF-LEAN")
        );
        assert_eq!(
            read(&repo.join("Tablet").join("Correct__Refutation.tex")).as_deref(),
            Some("REF-TEX")
        );
        assert!(!node_in_dormant(repo, &refutation));
        // Demote leg ran normally.
        assert!(!node_in_tablet(repo, &primary));
        assert_eq!(
            read(&repo.join("Dormant").join("Correct.lean")).as_deref(),
            Some("PRIMARY-LEAN")
        );
    }

    #[test]
    fn flip_refuses_divergent_dual_location_duplicate_without_touching_either_file() {
        // One-side invariant violated with DIVERGENT content: a stale seed
        // stub in Dormant/ must never clobber the accepted Tablet/ proof.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let primary = NodeId::from("Correct");
        let refutation = NodeId::from("Correct__Refutation");

        touch(&repo.join("Tablet").join("Correct.lean"), "PRIMARY-LEAN");
        touch(&repo.join("Tablet").join("Correct.tex"), "PRIMARY-TEX");
        // Accepted proof in Tablet/; stale divergent stub in Dormant/.
        touch(
            &repo.join("Tablet").join("Correct__Refutation.lean"),
            "ACCEPTED-REF-PROOF",
        );
        touch(
            &repo.join("Dormant").join("Correct__Refutation.lean"),
            "STALE-SEED-STUB",
        );

        let err = flip_decide_pair_on_disk(repo, &refutation, &primary).unwrap_err();
        // The error names BOTH paths and the remedy.
        assert!(
            err.contains("Correct__Refutation.lean"),
            "error must name the colliding file: {err}"
        );
        assert!(err.contains("Dormant"), "error must name the source path: {err}");
        assert!(err.contains("Tablet"), "error must name the destination path: {err}");
        assert!(
            err.contains("dual-location duplicate with divergent content")
                && err.contains("reconcile before flipping")
                && err.contains("refusing to overwrite"),
            "error must carry the remedy phrasing: {err}"
        );
        // NO file was modified by the failed call.
        assert_eq!(
            read(&repo.join("Tablet").join("Correct__Refutation.lean")).as_deref(),
            Some("ACCEPTED-REF-PROOF")
        );
        assert_eq!(
            read(&repo.join("Dormant").join("Correct__Refutation.lean")).as_deref(),
            Some("STALE-SEED-STUB")
        );
        assert_eq!(
            read(&repo.join("Tablet").join("Correct.lean")).as_deref(),
            Some("PRIMARY-LEAN")
        );
        assert_eq!(
            read(&repo.join("Tablet").join("Correct.tex")).as_deref(),
            Some("PRIMARY-TEX")
        );
    }

    #[test]
    fn torn_flip_rerun_with_absent_source_still_noops_cleanly() {
        // After a COMPLETED promote leg (torn before the demote), the re-run's
        // promote sources are absent: the overwrite guard must not fire (the
        // no-op precedes any destination check) and the flip completes.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let primary = NodeId::from("Correct");
        let refutation = NodeId::from("Correct__Refutation");

        // Post-promote torn state: both sides in Tablet/, Dormant/ empty.
        touch(&repo.join("Tablet").join("Correct.lean"), "PRIMARY-LEAN");
        touch(&repo.join("Tablet").join("Correct.tex"), "PRIMARY-TEX");
        touch(
            &repo.join("Tablet").join("Correct__Refutation.lean"),
            "REF-LEAN",
        );
        touch(
            &repo.join("Tablet").join("Correct__Refutation.tex"),
            "REF-TEX",
        );

        flip_decide_pair_on_disk(repo, &refutation, &primary).unwrap();
        assert!(node_in_tablet(repo, &refutation));
        assert!(!node_in_tablet(repo, &primary));
        assert!(node_in_dormant(repo, &primary));
        assert_eq!(
            read(&repo.join("Tablet").join("Correct__Refutation.lean")).as_deref(),
            Some("REF-LEAN")
        );
    }
}
