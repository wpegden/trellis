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

use crate::model::{ChallengePolarity, NodeId, ProtocolState};
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
/// CRASH-SAFETY. The flip performs four atomic per-file `rename`s in the order
/// new-live `.lean`, new-live `.tex`, old-live `.lean`, old-live `.tex`.
/// Consequently a crash can expose mixed `.lean`/`.tex` locations as well as
/// the both-nodes-in-Tablet midpoint. Runtime load recognizes only the closed
/// set of exact forward/recovery prefixes and restores the persisted-state
/// polarity via [`recover_interrupted_configured_decide_flips`]; arbitrary
/// missing, duplicate, non-regular, or divergent layouts still fail closed.
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

/// Fail-closed disk check for every configured `Decide` pair.
///
/// A healthy pair has exactly two regular `.lean`/`.tex` pairs: the polarity
/// selected by `ProtocolState::pv_live_polarity` exists only in `Tablet/`, and
/// its opposite exists only in `Dormant/`. This catches missing half-pairs,
/// duplicate copies, both-live, both-dormant, and non-regular path tricks
/// before another worker is dispatched against a state/disk disagreement.
pub fn validate_configured_decide_layout(
    repo_path: &Path,
    state: &ProtocolState,
) -> Result<(), String> {
    for primary in state.configured_challenge_targets.keys() {
        if !state.is_decide_primary(primary) {
            continue;
        }
        let live_polarity = state.live_polarity(primary);
        let dormant_polarity = match live_polarity {
            ChallengePolarity::Prove => ChallengePolarity::Disprove,
            ChallengePolarity::Disprove => ChallengePolarity::Prove,
        };
        let live = state
            .decide_pair_node_for_polarity(primary, live_polarity)
            .ok_or_else(|| {
                format!(
                    "configured Decide pair `{}` has no node for live polarity {:?}",
                    primary.as_str(),
                    live_polarity
                )
            })?;
        let dormant = state
            .decide_pair_node_for_polarity(primary, dormant_polarity)
            .ok_or_else(|| {
                format!(
                    "configured Decide pair `{}` has no node for dormant polarity {:?}",
                    primary.as_str(),
                    dormant_polarity
                )
            })?;
        // Match the engine's filename transform for namespaced declarations.
        let live = NodeId::from(live.replace('.', "_").as_str());
        let dormant = NodeId::from(dormant.replace('.', "_").as_str());
        let refutation = crate::model::refutation_target_id(primary);
        let state_materialized = [
            &state.live.present_nodes,
            &state.committed.present_nodes,
            &state.last_clean_live.present_nodes,
        ]
        .iter()
        .any(|present| present.contains(&live) || present.contains(&dormant));
        let state_covered = [
            &state.live.challenge_coverage,
            &state.committed.challenge_coverage,
            &state.approved_targets.challenge_coverage,
        ]
        .iter()
        .any(|coverage| {
            coverage
                .get(primary)
                .is_some_and(|nodes| !nodes.is_empty())
                || coverage
                    .get(&refutation)
                    .is_some_and(|nodes| !nodes.is_empty())
        });
        let disk_materialized = decide_pair_has_any_path(repo_path, &live, &dormant)?;

        // Config-only PV initialization deliberately registers both target
        // definitions before a worker authors either source node. That wholly
        // unmaterialized state is valid. The exemption closes permanently as
        // soon as state claims presence/coverage OR any one of the eight pair
        // paths appears; from then on the exact two-location layout is required.
        if !state_materialized && !state_covered && !disk_materialized {
            continue;
        }
        validate_decide_pair_layout(repo_path, primary.as_str(), &live, &dormant)?;
    }
    Ok(())
}

/// Recover the closed set of exact per-file topologies reachable when a
/// polarity flip, or recovery of that flip, crashes between renames. The
/// persisted state is authoritative, so recovery moves the files back to that
/// polarity. No other malformed layout is rewritten.
pub fn recover_interrupted_configured_decide_flips(
    repo_path: &Path,
    state: &ProtocolState,
) -> Result<bool, String> {
    let mut recovered = false;
    for primary in state.configured_challenge_targets.keys() {
        if !state.is_decide_primary(primary) {
            continue;
        }
        let persisted_live_polarity = state.live_polarity(primary);
        let persisted_dormant_polarity = match persisted_live_polarity {
            ChallengePolarity::Prove => ChallengePolarity::Disprove,
            ChallengePolarity::Disprove => ChallengePolarity::Prove,
        };
        let live = state
            .decide_pair_node_for_polarity(primary, persisted_live_polarity)
            .map(|name| NodeId::from(name.replace('.', "_").as_str()))
            .ok_or_else(|| format!("configured Decide pair `{}` has no live node", primary.as_str()))?;
        let dormant = state
            .decide_pair_node_for_polarity(primary, persisted_dormant_polarity)
            .map(|name| NodeId::from(name.replace('.', "_").as_str()))
            .ok_or_else(|| {
                format!(
                    "configured Decide pair `{}` has no dormant node",
                    primary.as_str()
                )
            })?;
        let live_layout = observe_node_layout(repo_path, &live)?;
        let dormant_layout = observe_node_layout(repo_path, &dormant)?;

        let expected = NodeDiskLayout::tablet_only();
        let parked = NodeDiskLayout::dormant_only();
        if live_layout == expected && dormant_layout == parked {
            continue;
        }
        // Forward flip order is new.lean, new.tex, old.lean, old.tex. Recovery
        // itself can also crash while reversing those moves, so admit the
        // closed union of exact forward prefixes and exact reverse prefixes.
        let split_new_lean_promoted = NodeDiskLayout::tablet_lean_dormant_tex();
        let split_old_lean_demoted = NodeDiskLayout::dormant_lean_tablet_tex();
        let recoverable_prefix = matches!(
            (live_layout, dormant_layout),
            (l, d)
                if (l == expected && d == split_new_lean_promoted)
                    || (l == expected && d == expected)
                    || (l == split_old_lean_demoted && d == expected)
                    || (l == parked && d == expected)
                    || (l == expected && d == split_old_lean_demoted)
                    || (l == split_new_lean_promoted && d == expected)
        );
        if recoverable_prefix {
            flip_decide_pair_on_disk(repo_path, &live, &dormant)?;
            recovered = true;
        }
    }
    Ok(recovered)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NodeDiskLayout {
    tablet_lean: bool,
    tablet_tex: bool,
    dormant_lean: bool,
    dormant_tex: bool,
}

impl NodeDiskLayout {
    fn tablet_only() -> Self {
        Self {
            tablet_lean: true,
            tablet_tex: true,
            dormant_lean: false,
            dormant_tex: false,
        }
    }

    fn dormant_only() -> Self {
        Self {
            tablet_lean: false,
            tablet_tex: false,
            dormant_lean: true,
            dormant_tex: true,
        }
    }

    fn tablet_lean_dormant_tex() -> Self {
        Self {
            tablet_lean: true,
            tablet_tex: false,
            dormant_lean: false,
            dormant_tex: true,
        }
    }

    fn dormant_lean_tablet_tex() -> Self {
        Self {
            tablet_lean: false,
            tablet_tex: true,
            dormant_lean: true,
            dormant_tex: false,
        }
    }
}

fn observe_node_layout(repo_path: &Path, node: &NodeId) -> Result<NodeDiskLayout, String> {
    let (tablet_lean, tablet_tex) = tablet_paths(repo_path, node);
    let (dormant_lean, dormant_tex) = dormant_paths(repo_path, node);
    Ok(NodeDiskLayout {
        tablet_lean: regular_file_present(&tablet_lean)?,
        tablet_tex: regular_file_present(&tablet_tex)?,
        dormant_lean: regular_file_present(&dormant_lean)?,
        dormant_tex: regular_file_present(&dormant_tex)?,
    })
}

fn path_entry_present(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "configured Decide layout cannot inspect {}: {error}",
            path.display()
        )),
    }
}

fn decide_pair_has_any_path(
    repo_path: &Path,
    first: &NodeId,
    second: &NodeId,
) -> Result<bool, String> {
    for node in [first, second] {
        let (tablet_lean, tablet_tex) = tablet_paths(repo_path, node);
        let (dormant_lean, dormant_tex) = dormant_paths(repo_path, node);
        for path in [tablet_lean, tablet_tex, dormant_lean, dormant_tex] {
            if path_entry_present(&path)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn regular_file_present(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(format!(
            "configured Decide layout path {} exists but is not a regular file",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "configured Decide layout cannot inspect {}: {error}",
            path.display()
        )),
    }
}

fn validate_node_location(
    pair_id: &str,
    node: &NodeId,
    expected_location: &str,
    tablet: (bool, bool),
    dormant: (bool, bool),
) -> Result<(), String> {
    let expected = match expected_location {
        "Tablet" => (true, true, false, false),
        "Dormant" => (false, false, true, true),
        _ => unreachable!("closed expected Decide location"),
    };
    let observed = (tablet.0, tablet.1, dormant.0, dormant.1);
    if observed != expected {
        return Err(format!(
            "configured Decide pair `{pair_id}` has invalid disk layout for node `{}`: \
             expected only {expected_location}/{{{}.lean,{}.tex}}; observed \
             Tablet(lean={},tex={}) Dormant(lean={},tex={})",
            node.as_str(),
            node.as_str(),
            node.as_str(),
            tablet.0,
            tablet.1,
            dormant.0,
            dormant.1,
        ));
    }
    Ok(())
}

fn validate_decide_pair_layout(
    repo_path: &Path,
    pair_id: &str,
    live: &NodeId,
    dormant: &NodeId,
) -> Result<(), String> {
    let (live_tablet_lean, live_tablet_tex) = tablet_paths(repo_path, live);
    let (live_dormant_lean, live_dormant_tex) = dormant_paths(repo_path, live);
    validate_node_location(
        pair_id,
        live,
        "Tablet",
        (
            regular_file_present(&live_tablet_lean)?,
            regular_file_present(&live_tablet_tex)?,
        ),
        (
            regular_file_present(&live_dormant_lean)?,
            regular_file_present(&live_dormant_tex)?,
        ),
    )?;

    let (dormant_tablet_lean, dormant_tablet_tex) = tablet_paths(repo_path, dormant);
    let (dormant_dormant_lean, dormant_dormant_tex) = dormant_paths(repo_path, dormant);
    validate_node_location(
        pair_id,
        dormant,
        "Dormant",
        (
            regular_file_present(&dormant_tablet_lean)?,
            regular_file_present(&dormant_tablet_tex)?,
        ),
        (
            regular_file_present(&dormant_dormant_lean)?,
            regular_file_present(&dormant_dormant_tex)?,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        refutation_target_id, ChallengeResolution, ChallengeTargetId, ChallengeTargetSpec,
    };

    fn touch(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn read(path: &Path) -> Option<String> {
        fs::read_to_string(path).ok()
    }

    fn decide_state(polarity: ChallengePolarity) -> ProtocolState {
        let primary = ChallengeTargetId::from("correct");
        let mut state = ProtocolState::default();
        state.configured_challenge_targets.insert(
            primary.clone(),
            ChallengeTargetSpec {
                name: "Correct".into(),
                resolution: ChallengeResolution::Decide,
                ..ChallengeTargetSpec::default()
            },
        );
        state.configured_challenge_targets.insert(
            refutation_target_id(&primary),
            ChallengeTargetSpec {
                name: "Correct__Refutation".into(),
                ..ChallengeTargetSpec::default()
            },
        );
        if polarity == ChallengePolarity::Disprove {
            state.pv_live_polarity.insert(primary, polarity);
        }
        state
    }

    fn seed_valid_decide_layout(repo: &Path, polarity: ChallengePolarity) {
        let (live_dir, live_name, dormant_dir, dormant_name) = match polarity {
            ChallengePolarity::Prove => ("Tablet", "Correct", "Dormant", "Correct__Refutation"),
            ChallengePolarity::Disprove => {
                ("Tablet", "Correct__Refutation", "Dormant", "Correct")
            }
        };
        for (dir, name, body) in [
            (live_dir, live_name, "LIVE"),
            (dormant_dir, dormant_name, "DORMANT"),
        ] {
            touch(&repo.join(dir).join(format!("{name}.lean")), body);
            touch(&repo.join(dir).join(format!("{name}.tex")), body);
        }
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

    #[test]
    fn configured_decide_layout_accepts_exact_layout_for_each_polarity() {
        for polarity in [ChallengePolarity::Prove, ChallengePolarity::Disprove] {
            let tmp = tempfile::tempdir().unwrap();
            seed_valid_decide_layout(tmp.path(), polarity);
            validate_configured_decide_layout(tmp.path(), &decide_state(polarity)).unwrap();
        }
    }

    #[test]
    fn configured_decide_layout_allows_only_wholly_unmaterialized_config_state() {
        let empty = tempfile::tempdir().unwrap();
        let state = decide_state(ChallengePolarity::Prove);
        validate_configured_decide_layout(empty.path(), &state).unwrap();

        let present = tempfile::tempdir().unwrap();
        let mut present_state = decide_state(ChallengePolarity::Prove);
        present_state.live.present_nodes.insert("Correct".into());
        assert!(validate_configured_decide_layout(present.path(), &present_state).is_err());

        let covered = tempfile::tempdir().unwrap();
        let mut covered_state = decide_state(ChallengePolarity::Prove);
        covered_state
            .live
            .challenge_coverage
            .insert(ChallengeTargetId::from("correct"), [NodeId::from("Correct")].into());
        assert!(validate_configured_decide_layout(covered.path(), &covered_state).is_err());

        let partial = tempfile::tempdir().unwrap();
        touch(&partial.path().join("Tablet/Correct.lean"), "PARTIAL");
        assert!(validate_configured_decide_layout(partial.path(), &state).is_err());
    }

    #[test]
    fn configured_decide_layout_rejects_missing_half_pair() {
        let tmp = tempfile::tempdir().unwrap();
        seed_valid_decide_layout(tmp.path(), ChallengePolarity::Disprove);
        fs::remove_file(tmp.path().join("Tablet/Correct__Refutation.tex")).unwrap();
        let error = validate_configured_decide_layout(
            tmp.path(),
            &decide_state(ChallengePolarity::Disprove),
        )
        .unwrap_err();
        assert!(error.contains("Tablet(lean=true,tex=false)"), "{error}");
    }

    #[test]
    fn configured_decide_layout_rejects_duplicate_both_live_and_both_dormant() {
        // Duplicate one side across Tablet/Dormant.
        let duplicate = tempfile::tempdir().unwrap();
        seed_valid_decide_layout(duplicate.path(), ChallengePolarity::Prove);
        touch(&duplicate.path().join("Dormant/Correct.lean"), "DUP");
        touch(&duplicate.path().join("Dormant/Correct.tex"), "DUP");
        assert!(validate_configured_decide_layout(
            duplicate.path(),
            &decide_state(ChallengePolarity::Prove),
        )
        .unwrap_err()
        .contains("Correct"));

        // Both logical sides live in Tablet/.
        let both_live = tempfile::tempdir().unwrap();
        for name in ["Correct", "Correct__Refutation"] {
            touch(&both_live.path().join(format!("Tablet/{name}.lean")), "X");
            touch(&both_live.path().join(format!("Tablet/{name}.tex")), "X");
        }
        assert!(validate_configured_decide_layout(
            both_live.path(),
            &decide_state(ChallengePolarity::Prove),
        )
        .is_err());

        // Both logical sides dormant in Dormant/.
        let both_dormant = tempfile::tempdir().unwrap();
        for name in ["Correct", "Correct__Refutation"] {
            touch(
                &both_dormant.path().join(format!("Dormant/{name}.lean")),
                "X",
            );
            touch(
                &both_dormant.path().join(format!("Dormant/{name}.tex")),
                "X",
            );
        }
        assert!(validate_configured_decide_layout(
            both_dormant.path(),
            &decide_state(ChallengePolarity::Prove),
        )
        .is_err());
    }

    #[test]
    fn interrupted_flip_recovery_reverts_every_forward_rename_prefix() {
        let state = decide_state(ChallengePolarity::Prove);
        let renames = [
            ("Dormant/Correct__Refutation.lean", "Tablet/Correct__Refutation.lean"),
            ("Dormant/Correct__Refutation.tex", "Tablet/Correct__Refutation.tex"),
            ("Tablet/Correct.lean", "Dormant/Correct.lean"),
            ("Tablet/Correct.tex", "Dormant/Correct.tex"),
        ];
        for prefix_len in 1..=renames.len() {
            let tmp = tempfile::tempdir().unwrap();
            seed_valid_decide_layout(tmp.path(), ChallengePolarity::Prove);
            for (from, to) in renames.iter().take(prefix_len) {
                fs::rename(tmp.path().join(from), tmp.path().join(to)).unwrap();
            }
            assert!(recover_interrupted_configured_decide_flips(tmp.path(), &state).unwrap());
            validate_configured_decide_layout(tmp.path(), &state).unwrap();
        }
    }

    #[test]
    fn interrupted_recovery_is_itself_rerunnable_from_each_reverse_prefix() {
        let state = decide_state(ChallengePolarity::Prove);

        // Reverse prefix reached while undoing forward prefix 2: new `.lean`
        // already returned to Dormant, new `.tex` still in Tablet.
        let new_split = tempfile::tempdir().unwrap();
        seed_valid_decide_layout(new_split.path(), ChallengePolarity::Prove);
        fs::rename(
            new_split.path().join("Dormant/Correct__Refutation.tex"),
            new_split.path().join("Tablet/Correct__Refutation.tex"),
        )
        .unwrap();
        assert!(recover_interrupted_configured_decide_flips(new_split.path(), &state).unwrap());
        validate_configured_decide_layout(new_split.path(), &state).unwrap();

        // Reverse prefix reached while undoing a completed flip: old `.lean`
        // promoted, old `.tex` still dormant, new side still wholly Tablet.
        let old_split = tempfile::tempdir().unwrap();
        seed_valid_decide_layout(old_split.path(), ChallengePolarity::Prove);
        flip_decide_pair_on_disk(
            old_split.path(),
            &NodeId::from("Correct__Refutation"),
            &NodeId::from("Correct"),
        )
        .unwrap();
        fs::rename(
            old_split.path().join("Dormant/Correct.lean"),
            old_split.path().join("Tablet/Correct.lean"),
        )
        .unwrap();
        assert!(recover_interrupted_configured_decide_flips(old_split.path(), &state).unwrap());
        validate_configured_decide_layout(old_split.path(), &state).unwrap();
    }

    #[test]
    fn interrupted_flip_recovery_does_not_rewrite_arbitrary_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let state = decide_state(ChallengePolarity::Prove);
        seed_valid_decide_layout(tmp.path(), ChallengePolarity::Prove);
        touch(&tmp.path().join("Dormant/Correct.lean"), "DUPLICATE");
        touch(&tmp.path().join("Dormant/Correct.tex"), "DUPLICATE");

        assert!(!recover_interrupted_configured_decide_flips(tmp.path(), &state).unwrap());
        assert!(validate_configured_decide_layout(tmp.path(), &state).is_err());
        assert_eq!(
            read(&tmp.path().join("Dormant/Correct.lean")).as_deref(),
            Some("DUPLICATE")
        );
    }

    #[test]
    fn recovery_completes_flip_forward_when_persisted_polarity_leads_disk() {
        // Persisted state has already advanced to Disprove, but the crash left
        // the disk showing the OLD polarity in full (a complete Prove-live
        // layout — the `(parked, expected)` recovery prefix). Recovery drives
        // the disk FORWARD to the persisted polarity.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        seed_valid_decide_layout(repo, ChallengePolarity::Prove);
        let state = decide_state(ChallengePolarity::Disprove);

        assert!(recover_interrupted_configured_decide_flips(repo, &state).unwrap());

        let primary = NodeId::from("Correct");
        let refutation = NodeId::from("Correct__Refutation");
        // Disprove-live: refutation live in Tablet/, primary dormant in
        // Dormant/, every file in exactly one location.
        assert!(node_in_tablet(repo, &refutation));
        assert!(!node_in_dormant(repo, &refutation));
        assert!(node_in_dormant(repo, &primary));
        assert!(!node_in_tablet(repo, &primary));
        assert_eq!(
            read(&repo.join("Tablet/Correct__Refutation.lean")).as_deref(),
            Some("DORMANT")
        );
        assert_eq!(
            read(&repo.join("Dormant/Correct.lean")).as_deref(),
            Some("LIVE")
        );
        validate_configured_decide_layout(repo, &state).unwrap();
    }

    #[test]
    fn configured_decide_layout_rejects_symlinked_pair_path() {
        // A materialized, otherwise-valid pair where one EXPECTED regular file
        // is a symlink pointing at a regular file with the correct content.
        // The one-location invariant demands a regular file at the pair path;
        // a non-regular entry fails loud and mutates nothing.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        seed_valid_decide_layout(repo, ChallengePolarity::Prove);

        let pair_path = repo.join("Tablet/Correct.lean");
        let content = read(&pair_path).unwrap();
        // A regular file with the SAME content lives outside the pair paths;
        // the symlink at the expected location points at it.
        let sidecar = repo.join("sidecar_Correct.lean");
        touch(&sidecar, &content);
        fs::remove_file(&pair_path).unwrap();
        std::os::unix::fs::symlink(&sidecar, &pair_path).unwrap();

        let error = validate_configured_decide_layout(repo, &decide_state(ChallengePolarity::Prove))
            .unwrap_err();
        assert!(
            error.contains("is not a regular file"),
            "symlinked pair path must fail loud: {error}"
        );
        assert!(error.contains("Correct.lean"), "error must name the path: {error}");

        // Nothing was mutated: the symlink still resolves to the sidecar's
        // correct content and the sidecar is intact.
        assert_eq!(fs::read_to_string(&pair_path).unwrap(), content);
        assert_eq!(fs::read_to_string(&sidecar).unwrap(), content);
        assert!(fs::symlink_metadata(&pair_path).unwrap().file_type().is_symlink());
    }
}
