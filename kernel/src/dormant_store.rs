//! Minimal stub: the program-verification under-model support is not included
//! in this public release.
//!
//! The public interface below is retained so the ~kept call sites across the
//! kernel continue to compile and behave correctly for non-PV runs. The two
//! directories `Tablet/` and `Dormant/` are moved between by a polarity flip;
//! non-PV runs never create a `Dormant/` directory, so these helpers are inert
//! there.

use crate::model::NodeId;
use std::fs;
use std::path::{Path, PathBuf};

/// The dormant-store directory beside `Tablet/`.
pub fn dormant_dir(repo_path: &Path) -> PathBuf {
    repo_path.join("Dormant")
}

fn tablet_dir(repo_path: &Path) -> PathBuf {
    repo_path.join("Tablet")
}

fn tablet_paths(repo_path: &Path, node: &NodeId) -> (PathBuf, PathBuf) {
    let dir = tablet_dir(repo_path);
    (
        dir.join(format!("{}.lean", node.as_str())),
        dir.join(format!("{}.tex", node.as_str())),
    )
}

fn dormant_paths(repo_path: &Path, node: &NodeId) -> (PathBuf, PathBuf) {
    let dir = dormant_dir(repo_path);
    (
        dir.join(format!("{}.lean", node.as_str())),
        dir.join(format!("{}.tex", node.as_str())),
    )
}

/// True iff `node` has a `.lean` file in `Tablet/` (currently live).
pub fn node_in_tablet(repo_path: &Path, node: &NodeId) -> bool {
    tablet_paths(repo_path, node).0.exists()
}

/// True iff `node` has a `.lean` file in `Dormant/` (currently dormant on disk).
pub fn node_in_dormant(repo_path: &Path, node: &NodeId) -> bool {
    dormant_paths(repo_path, node).0.exists()
}

fn move_if_present(from: &Path, to: &Path) -> Result<(), String> {
    if !from.exists() {
        return Ok(());
    }
    fs::rename(from, to).map_err(|err| {
        format!(
            "dormant flip: failed to move {} -> {}: {err}",
            from.display(),
            to.display()
        )
    })
}

fn promote_to_tablet(repo_path: &Path, node: &NodeId) -> Result<(), String> {
    fs::create_dir_all(tablet_dir(repo_path))
        .map_err(|err| format!("dormant flip: failed to ensure Tablet/: {err}"))?;
    let (dorm_lean, dorm_tex) = dormant_paths(repo_path, node);
    let (tab_lean, tab_tex) = tablet_paths(repo_path, node);
    move_if_present(&dorm_lean, &tab_lean)?;
    move_if_present(&dorm_tex, &tab_tex)?;
    Ok(())
}

fn demote_to_dormant(repo_path: &Path, node: &NodeId) -> Result<(), String> {
    fs::create_dir_all(dormant_dir(repo_path))
        .map_err(|err| format!("dormant flip: failed to ensure Dormant/: {err}"))?;
    let (tab_lean, tab_tex) = tablet_paths(repo_path, node);
    let (dorm_lean, dorm_tex) = dormant_paths(repo_path, node);
    move_if_present(&tab_lean, &dorm_lean)?;
    move_if_present(&tab_tex, &dorm_tex)?;
    Ok(())
}

/// Move the newly-live node `Dormant/ -> Tablet/` and the newly-dormant node
/// `Tablet/ -> Dormant/`. Promote-then-demote, and re-runnable.
pub fn flip_decide_pair_on_disk(
    repo_path: &Path,
    newly_live: &NodeId,
    newly_dormant: &NodeId,
) -> Result<(), String> {
    promote_to_tablet(repo_path, newly_live)?;
    demote_to_dormant(repo_path, newly_dormant)?;
    Ok(())
}
