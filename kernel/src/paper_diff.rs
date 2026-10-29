//! Deterministic paper diff for revision mode (`revision_plan.md` §6).
//!
//! Compares the old and new paper sources of a revision run and classifies
//! each labeled statement block as Unchanged / Changed / Added / Removed.
//! Matching is by TeX label only; block equality is decided by a
//! whitespace-normalized SHA-256 hash, so a purely cosmetic edit (e.g.
//! "Suppose D" -> "Suppose that D") reports `Unchanged` and does not
//! spuriously invalidate a target. No fuzzy matching is used for decisions
//! that carry approvals (§6, §18).
//!
//! Unlabeled, line-based targets are handled by `line_target_delta`, which
//! carries an approval (`Unchanged`) only when the same old line range is
//! still present in the new paper with an identical text hash.

use crate::model::{RevisionTargetDelta, RevisionTargetDeltaKind};
use crate::paper_targets::{extract_paper_statement_blocks, PaperStatementBlock};
use crate::TargetId;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Stable `TargetId` for a labeled target. Uses the bare label so it matches
/// the label-keyed configured-target convention (`thm:main`), and stays
/// stable across paper revisions even when the block moves to new lines.
pub fn label_target_id(label: &str) -> TargetId {
    TargetId::from(label.trim())
}

/// Collapse every run of ASCII whitespace (spaces, tabs, newlines) to a
/// single space and trim the ends. This is the §6 normalization: a cosmetic
/// reflow or single-word insertion that does not change the mathematical
/// content still hashes identically only when the words are unchanged, but
/// pure whitespace churn cancels.
pub fn normalize_block_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_ws = false;
    for ch in text.chars() {
        if ch.is_ascii_whitespace() {
            in_ws = true;
            continue;
        }
        if in_ws && !out.is_empty() {
            out.push(' ');
        }
        in_ws = false;
        out.push(ch);
    }
    out
}

/// SHA-256 of the whitespace-normalized block text.
pub fn hash_block(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalize_block_text(text).as_bytes());
    format!("{:x}", hasher.finalize())
}

/// First non-empty label of a block, if any.
fn block_label(block: &PaperStatementBlock) -> Option<&str> {
    block
        .labels
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .find(|label| !label.is_empty())
}

/// Index labeled blocks by their first label, failing on a duplicate label
/// within a single paper (a duplicate label makes label matching ambiguous,
/// so it must be a deterministic error rather than an arbitrary pick — §15).
fn index_blocks_by_label<'a>(
    blocks: &'a [PaperStatementBlock],
    which: &str,
) -> Result<BTreeMap<String, &'a PaperStatementBlock>, String> {
    let mut by_label: BTreeMap<String, &PaperStatementBlock> = BTreeMap::new();
    for block in blocks {
        if let Some(label) = block_label(block) {
            if by_label.insert(label.to_string(), block).is_some() {
                return Err(format!(
                    "duplicate TeX label `{label}` in {which} paper; cannot match deterministically"
                ));
            }
        }
    }
    Ok(by_label)
}

/// Build a `RevisionTargetDelta` for a labeled target from the matched
/// old/new blocks.
fn labeled_delta(
    label: &str,
    old: Option<&PaperStatementBlock>,
    new: Option<&PaperStatementBlock>,
) -> RevisionTargetDelta {
    let old_hash = old.map(|b| hash_block(&b.text));
    let new_hash = new.map(|b| hash_block(&b.text));
    let kind = match (&old_hash, &new_hash) {
        (Some(o), Some(n)) if o == n => RevisionTargetDeltaKind::Unchanged,
        (Some(_), Some(_)) => RevisionTargetDeltaKind::Changed,
        (None, Some(_)) => RevisionTargetDeltaKind::Added,
        (Some(_), None) => RevisionTargetDeltaKind::Removed,
        (None, None) => RevisionTargetDeltaKind::Removed,
    };
    RevisionTargetDelta {
        target: label_target_id(label),
        label: Some(label.to_string()),
        old_block_hash: old_hash,
        new_block_hash: new_hash,
        old_start_line: old.map(|b| b.start_line),
        old_end_line: old.map(|b| b.end_line),
        new_start_line: new.map(|b| b.start_line),
        new_end_line: new.map(|b| b.end_line),
        kind,
    }
}

/// Diff the labeled statement blocks of the old and new paper sources. Every
/// label present in either paper produces one delta keyed by its
/// `label_target_id`. Returns a deterministic error on a duplicate label in
/// either paper.
pub fn diff_labeled_targets(
    old_paper_text: &str,
    new_paper_text: &str,
) -> Result<BTreeMap<TargetId, RevisionTargetDelta>, String> {
    let old_blocks = extract_paper_statement_blocks(old_paper_text, None);
    let new_blocks = extract_paper_statement_blocks(new_paper_text, None);
    let old_by_label = index_blocks_by_label(&old_blocks, "old")?;
    let new_by_label = index_blocks_by_label(&new_blocks, "new")?;

    let all_labels: BTreeSet<&String> = old_by_label.keys().chain(new_by_label.keys()).collect();
    let mut deltas = BTreeMap::new();
    for label in all_labels {
        let delta = labeled_delta(
            label,
            old_by_label.get(label).copied(),
            new_by_label.get(label).copied(),
        );
        deltas.insert(delta.target.clone(), delta);
    }
    Ok(deltas)
}

/// Delta for a single unlabeled, line-based target. The target carries its
/// paper-faithfulness approval (`Unchanged`) only when the same old line
/// range is still present in the new paper AND the block text hash is
/// identical (§6 rule 2). Otherwise it is `Changed`. A line-based target is
/// never matched fuzzily, so a moved or edited block reports `Changed` and
/// loses its approval, which is the conservative outcome the plan mandates.
pub fn line_target_delta(
    target: TargetId,
    old_start_line: i64,
    old_end_line: i64,
    old_paper_text: &str,
    new_paper_text: &str,
) -> RevisionTargetDelta {
    let old_blocks = extract_paper_statement_blocks(old_paper_text, None);
    let new_blocks = extract_paper_statement_blocks(new_paper_text, None);
    let find_by_range = |blocks: &[PaperStatementBlock], start: i64, end: i64| {
        blocks
            .iter()
            .find(|b| b.start_line == start && b.end_line == end)
            .map(|b| b.text.clone())
    };
    let old_text = find_by_range(&old_blocks, old_start_line, old_end_line);
    let old_hash = old_text.as_deref().map(hash_block);
    // Match the new block by identical range only (no fuzzy fallback).
    let new_text = find_by_range(&new_blocks, old_start_line, old_end_line);
    let new_hash = new_text.as_deref().map(hash_block);

    let kind = match (&old_hash, &new_hash) {
        (Some(o), Some(n)) if o == n => RevisionTargetDeltaKind::Unchanged,
        (Some(_), _) => RevisionTargetDeltaKind::Changed,
        (None, _) => RevisionTargetDeltaKind::Changed,
    };
    RevisionTargetDelta {
        target,
        label: None,
        old_block_hash: old_hash,
        new_block_hash: new_hash,
        old_start_line: Some(old_start_line),
        old_end_line: Some(old_end_line),
        new_start_line: if new_text.is_some() {
            Some(old_start_line)
        } else {
            None
        },
        new_end_line: if new_text.is_some() {
            Some(old_end_line)
        } else {
            None
        },
        kind,
    }
}

/// Compact, human-readable summary of a set of target deltas, for the
/// revision planner packet and HumanGate (§6, §9, §11). Lists changed,
/// added, and removed labels; unchanged targets are summarized by count.
pub fn revision_diff_report(deltas: &BTreeMap<TargetId, RevisionTargetDelta>) -> String {
    let mut changed = Vec::new();
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut unchanged = 0usize;
    for delta in deltas.values() {
        let name = delta
            .label
            .clone()
            .unwrap_or_else(|| delta.target.as_str().to_string());
        match delta.kind {
            RevisionTargetDeltaKind::Changed => changed.push(name),
            RevisionTargetDeltaKind::Added => added.push(name),
            RevisionTargetDeltaKind::Removed => removed.push(name),
            RevisionTargetDeltaKind::Unchanged => unchanged += 1,
        }
    }
    let mut report = String::new();
    report.push_str(&format!(
        "Paper diff: {} changed, {} added, {} removed, {} unchanged.\n",
        changed.len(),
        added.len(),
        removed.len(),
        unchanged
    ));
    if !changed.is_empty() {
        report.push_str(&format!("  changed: {}\n", changed.join(", ")));
    }
    if !added.is_empty() {
        report.push_str(&format!("  added: {}\n", added.join(", ")));
    }
    if !removed.is_empty() {
        report.push_str(&format!("  removed: {}\n", removed.join(", ")));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(body: &str) -> String {
        format!("\\begin{{document}}\n{body}\n\\end{{document}}\n")
    }

    #[test]
    fn labeled_theorem_moved_to_different_lines_is_matched_by_label() {
        let old = doc("\\begin{theorem}\\label{thm:main}\nStatement A.\n\\end{theorem}");
        // Same label, same content, but pushed down by padding lines.
        let new = doc(
            "\\begin{lemma}\\label{lem:pad}\nPadding.\n\\end{lemma}\n\
             \\begin{theorem}\\label{thm:main}\nStatement A.\n\\end{theorem}",
        );
        let deltas = diff_labeled_targets(&old, &new).expect("diff");
        let d = &deltas[&label_target_id("thm:main")];
        assert_eq!(d.kind, RevisionTargetDeltaKind::Unchanged);
        assert_ne!(d.old_start_line, d.new_start_line, "block did move lines");
    }

    #[test]
    fn labeled_theorem_with_changed_body_is_changed() {
        let old = doc("\\begin{theorem}\\label{thm:main}\nFor s>=4 the bound holds.\n\\end{theorem}");
        let new = doc("\\begin{theorem}\\label{thm:main}\nFor s>=3 the bound holds.\n\\end{theorem}");
        let deltas = diff_labeled_targets(&old, &new).expect("diff");
        assert_eq!(
            deltas[&label_target_id("thm:main")].kind,
            RevisionTargetDeltaKind::Changed
        );
    }

    #[test]
    fn new_labeled_theorem_is_added() {
        let old = doc("\\begin{theorem}\\label{thm:main}\nMain.\n\\end{theorem}");
        let new = doc(
            "\\begin{theorem}\\label{thm:main}\nMain.\n\\end{theorem}\n\
             \\begin{theorem}\\label{thm:s-2}\nWeak.\n\\end{theorem}",
        );
        let deltas = diff_labeled_targets(&old, &new).expect("diff");
        assert_eq!(
            deltas[&label_target_id("thm:s-2")].kind,
            RevisionTargetDeltaKind::Added
        );
        assert!(deltas[&label_target_id("thm:s-2")].old_block_hash.is_none());
    }

    #[test]
    fn old_labeled_theorem_absent_from_new_is_removed() {
        let old = doc(
            "\\begin{theorem}\\label{thm:main}\nMain.\n\\end{theorem}\n\
             \\begin{lemma}\\label{lem:gone}\nDropped.\n\\end{lemma}",
        );
        let new = doc("\\begin{theorem}\\label{thm:main}\nMain.\n\\end{theorem}");
        let deltas = diff_labeled_targets(&old, &new).expect("diff");
        assert_eq!(
            deltas[&label_target_id("lem:gone")].kind,
            RevisionTargetDeltaKind::Removed
        );
        assert!(deltas[&label_target_id("lem:gone")].new_block_hash.is_none());
    }

    #[test]
    fn cosmetic_whitespace_edit_is_unchanged() {
        // A real-paper example: "Suppose D" -> "Suppose that D" is a real
        // word change (Changed), but a pure whitespace reflow is Unchanged.
        let old = doc("\\begin{lemma}\\label{lem:x}\nSuppose D holds.\n\\end{lemma}");
        let new = doc("\\begin{lemma}\\label{lem:x}\nSuppose D    holds.\n\n\\end{lemma}");
        let deltas = diff_labeled_targets(&old, &new).expect("diff");
        assert_eq!(
            deltas[&label_target_id("lem:x")].kind,
            RevisionTargetDeltaKind::Unchanged,
            "pure whitespace churn must normalize to Unchanged"
        );
    }

    #[test]
    fn unlabeled_line_target_carries_only_on_identical_hash() {
        let old = doc("\\begin{theorem}\nAnon statement.\n\\end{theorem}");
        // theorem starts at line 2, ends at line 4 inside the document.
        let blocks = extract_paper_statement_blocks(&old, None);
        let b = &blocks[0];
        let same = doc("\\begin{theorem}\nAnon statement.\n\\end{theorem}");
        let d_same = line_target_delta(
            TargetId::from("anon"),
            b.start_line,
            b.end_line,
            &old,
            &same,
        );
        assert_eq!(d_same.kind, RevisionTargetDeltaKind::Unchanged);

        let edited = doc("\\begin{theorem}\nDifferent statement.\n\\end{theorem}");
        let d_edited = line_target_delta(
            TargetId::from("anon"),
            b.start_line,
            b.end_line,
            &old,
            &edited,
        );
        assert_eq!(d_edited.kind, RevisionTargetDeltaKind::Changed);
    }

    #[test]
    fn duplicate_labels_produce_a_deterministic_error() {
        let dup = doc(
            "\\begin{theorem}\\label{thm:dup}\nA.\n\\end{theorem}\n\
             \\begin{lemma}\\label{thm:dup}\nB.\n\\end{lemma}",
        );
        let clean = doc("\\begin{theorem}\\label{thm:main}\nM.\n\\end{theorem}");
        let err = diff_labeled_targets(&dup, &clean).unwrap_err();
        assert!(err.contains("duplicate TeX label"), "got: {err}");
        assert!(err.contains("thm:dup"), "got: {err}");
    }

    #[test]
    fn report_lists_changed_added_removed() {
        let old = doc(
            "\\begin{theorem}\\label{thm:main}\ns>=4.\n\\end{theorem}\n\
             \\begin{lemma}\\label{lem:gone}\nDropped.\n\\end{lemma}\n\
             \\begin{lemma}\\label{lem:keep}\nKept.\n\\end{lemma}",
        );
        let new = doc(
            "\\begin{theorem}\\label{thm:main}\ns>=3.\n\\end{theorem}\n\
             \\begin{lemma}\\label{lem:keep}\nKept.\n\\end{lemma}\n\
             \\begin{theorem}\\label{thm:s-2}\nNew.\n\\end{theorem}",
        );
        let deltas = diff_labeled_targets(&old, &new).expect("diff");
        let report = revision_diff_report(&deltas);
        assert!(report.contains("1 changed"));
        assert!(report.contains("1 added"));
        assert!(report.contains("1 removed"));
        assert!(report.contains("1 unchanged"));
        assert!(report.contains("thm:main"));
        assert!(report.contains("thm:s-2"));
        assert!(report.contains("lem:gone"));
    }
}
