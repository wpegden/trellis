//! Source-facing NL preservation arguments attached to existing model nodes.
//! They use the ordinary Soundness evidence lifecycle, independently of GOAL
//! proofs. No extra Lean semantics or synthetic theorem is introduced.
use std::{collections::{BTreeMap, BTreeSet}, fs, path::Path};
use crate::{NodeId, NodeKind, PvRole, TargetId};

pub const SCHEMA: &str = "trellis-preservation/v1";
const BEGIN: &str = "\\begin{preservation}";
const END: &str = "\\end{preservation}";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub statement: String,
    pub proof: String,
    pub goals: BTreeMap<String, String>,
}

fn block<'a>(input: &'a str, env: &str) -> Result<(&'a str, &'a str), String> {
    let begin = format!("\\begin{{{env}}}");
    let end = format!("\\end{{{env}}}");
    let rest = input.trim_start().strip_prefix(&begin)
        .ok_or_else(|| format!("preservation: expected {begin}"))?;
    rest.split_once(&end).ok_or_else(|| format!("preservation: missing {end}"))
}

/// The base TeX remains the ordinary paired statement/proof surface.
pub fn split(tex: &str) -> Result<(String, Vec<Claim>), String> {
    let Some((base, mut rest)) = tex.split_once(BEGIN) else {
        return Ok((tex.to_string(), Vec::new()));
    };
    let mut claims = Vec::new();
    loop {
        let (body, tail) = rest.split_once(END)
            .ok_or("preservation: missing end of section")?;
        let (statement, after_statement) = block(body, "claim")?;
        if statement.trim().is_empty() { return Err("preservation claim is empty".into()); }
        let (proof, mut after_proof) = block(after_statement, "proof")?;
        let mut goals = BTreeMap::new();
        while !after_proof.trim().is_empty() {
            let (application, tail) = block(after_proof, "goalproof")?;
            let app = application.trim_start().strip_prefix('{')
                .ok_or("goalproof requires a braced supplied GOAL ID")?;
            let (goal, text) = app.split_once('}').ok_or("goalproof has an unclosed GOAL ID")?;
            if goal.is_empty() || goals.insert(goal.to_string(), text.trim().to_string()).is_some() {
                return Err(format!("duplicate or empty preservation GOAL ID: {goal}"));
            }
            after_proof = tail;
        }
        claims.push(Claim { statement: statement.trim().into(), proof: proof.trim().into(), goals });
        if tail.trim().is_empty() { break; }
        rest = tail.trim_start().strip_prefix(BEGIN)
            .ok_or("only preservation sections may follow a preservation section")?;
    }
    Ok((base.to_string(), claims))
}

pub fn enabled(repo: &Path) -> bool {
    fs::read_to_string(repo.join("modeling-input/manifest.json")).ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .is_some_and(|v| v["preservation_schema"] == SCHEMA)
}

pub fn owner(kind: Option<&NodeKind>, role: Option<&PvRole>) -> bool {
    matches!(kind, Some(NodeKind::Definition | NodeKind::Preamble))
        && !matches!(role, Some(PvRole::ExtractionModel | PvRole::UnderModelAssumptions))
}

pub fn incomplete(tex: &str, goals: &BTreeSet<TargetId>) -> bool {
    let Ok((_, claims)) = split(tex) else { return true; };
    let unfinished = |p: &str| p.trim().is_empty() || p.lines().find(|l| !l.trim().is_empty())
        .is_some_and(|l| l.trim() == "SKETCH:");
    claims.is_empty() || claims.iter().any(|c| unfinished(&c.proof)
        || goals.iter().any(|g| c.goals.get(g.as_str()).is_none_or(|p| unfinished(p))))
}

pub fn noderefs(text: &str) -> BTreeSet<String> {
    let mut refs = BTreeSet::new();
    let mut rest = text;
    while let Some((_, tail)) = rest.split_once("\\noderef") {
        rest = tail;
        if let Some(tail) = rest.trim_start().strip_prefix('{') {
            if let Some((name, tail)) = tail.split_once('}') {
                if !name.trim().is_empty() { refs.insert(name.trim().to_string()); }
                rest = tail;
            }
        }
    }
    refs
}

pub fn references(tex: &str) -> BTreeSet<String> {
    split(tex).map(|(_, claims)| claims.into_iter().flat_map(|c| {
        let mut refs = noderefs(&c.statement);
        refs.extend(noderefs(&c.proof));
        for proof in c.goals.values() { refs.extend(noderefs(proof)); }
        refs
    }).collect()).unwrap_or_default()
}

/// Source claims have NL dependencies, not additional Lean import edges.
/// A definition's preservation theorem may naturally import that definition.
pub fn reference_errors(repo: &Path, node: &str, tex: &str) -> Vec<String> {
    let mut errors: Vec<String> = references(tex).into_iter().filter_map(|reference| {
        if reference == node { return None; } // The owner's paired definition.
        let root = repo.join("Tablet");
        if root.join(format!("{reference}.lean")).is_file()
            && root.join(format!("{reference}.tex")).is_file() { None }
        else { Some(format!("preservation in {node}: missing paired support node {reference}")) }
    }).collect();
    match evidence_references(tex) {
        Ok(paths) => for path in paths {
            if let Err(error) = evidence_bytes(repo, &path) {
                errors.push(format!("preservation in {node}: {error}"));
            }
        },
        Err(error) => errors.push(format!("preservation in {node}: {error}")),
    }
    errors
}

/// Explicit primary-evidence attachments. Keep this small grammar in sync with
/// viewer/pv_expert_package.js; both check normalized paths and reject symlinks.
pub fn evidence_references(tex: &str) -> Result<BTreeSet<String>, String> {
    let text = tex.lines().map(|line| {
        let mut escaped = false;
        for (index, ch) in line.char_indices() {
            if ch == '%' && !escaped { return &line[..index]; }
            escaped = ch == '\\' && !escaped;
        }
        line
    }).collect::<Vec<_>>().join("\n");
    let mut refs = BTreeSet::new();
    let mut rest = text.as_str();
    while let Some((_, tail)) = rest.split_once("\\evidenceref") {
        rest = tail;
        if tail.starts_with(|c: char| c.is_ascii_alphabetic()) { continue; }
        let tail = tail.trim_start().strip_prefix('{')
            .ok_or("evidenceref requires a braced file path")?;
        let (path, tail) = tail.split_once('}').ok_or("unclosed evidenceref file path")?;
        validate_evidence_path(path)?;
        refs.insert(path.to_string());
        rest = tail;
    }
    Ok(refs)
}

pub(crate) fn validate_evidence_path(path: &str) -> Result<(), String> {
    if !path.starts_with("reference/model-evidence/")
        || path.split('/').any(|part| part.is_empty() || part == "." || part == "..")
        || !path.bytes().all(|b| b.is_ascii_alphanumeric() || b"_./-".contains(&b)) {
        return Err(format!("invalid evidence path {path:?}: use a normalized file path under reference/model-evidence/ (letters, digits, _, -, ., /)"));
    }
    Ok(())
}

pub fn evidence_bytes(repo: &Path, path: &str) -> Result<Vec<u8>, String> {
    validate_evidence_path(path)?;
    let mut file = repo.to_path_buf();
    for part in path.split('/') {
        file.push(part);
        if file.is_symlink() { return Err(format!("evidence path uses a symlink: {path}")); }
    }
    if !file.is_file() { return Err(format!("missing evidence file: {path}")); }
    fs::read(&file).map_err(|err| format!("unreadable evidence file {path}: {err}"))
}

/// Empty means the old fingerprint is preserved. Only the citing subject's
/// Sound evidence depends on these bytes; ordinary node meaning is unchanged.
pub fn evidence_fingerprint_suffix(repo: &Path, tex: &str) -> String {
    match evidence_references(tex) {
        Ok(paths) if paths.is_empty() => String::new(),
        Ok(paths) => {
            let hashes: BTreeMap<_, _> = paths.into_iter().map(|path| {
                let hash = evidence_bytes(repo, &path).map(|bytes| crate::cache_key::hash_bytes(&bytes));
                (path, hash)
            }).collect();
            format!("\0primary-evidence-v1:{}", serde_json::to_string(&hashes).unwrap())
        },
        Err(error) => format!("\0invalid-primary-evidence:{error}"),
    }
}

/// Only claims, not their proofs, affect GOAL-meaning verification.
pub fn statements(tex: &str) -> String {
    split(tex).map(|(_, claims)| claims.into_iter().map(|c| c.statement)
        .collect::<Vec<_>>().join("\n")).unwrap_or_default()
}

/// Bind external evidence to the actual frozen inputs and support composition.
pub fn basis(repo: &Path) -> String {
    fn visit(root: &Path, path: &Path, out: &mut BTreeMap<String, String>) {
        if path.is_symlink() || path.file_name().is_some_and(|n|
            matches!(n.to_str(), Some(".git" | "target" | ".lake" | ".trellis"))) { return; }
        if path.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() { visit(root, &entry.path(), out); }
            }
        } else if let Ok(bytes) = fs::read(path) {
            out.insert(path.strip_prefix(root).unwrap_or(path).to_string_lossy().into(),
                crate::cache_key::hash_bytes(&bytes));
        }
    }
    let mut files = BTreeMap::new();
    let paper_basis = crate::paper_implementation::observed_subject_basis(repo);
    for name in ["GOAL.md", "modeling-input", "upstream", "crate", "generated.lean",
        "Tablet/model-support.json", "Tablet/Preamble.lean"] {
        if name == "GOAL.md" && !matches!(&paper_basis, Ok(None)) { continue; }
        visit(repo, &repo.join(name), &mut files);
    }
    match paper_basis {
        Ok(Some(subject)) => { files.insert("paper-implementation-subject".into(), subject); }
        Err(error) => { files.insert("paper-implementation-invalid".into(), error); }
        Ok(None) => {}
    }
    crate::cache_key::hash_text(&serde_json::to_string(&files).unwrap())
}

pub fn missing_nodes(repo: &Path, nodes: &BTreeSet<NodeId>, kinds: &BTreeMap<NodeId, NodeKind>,
    roles: &BTreeMap<NodeId, PvRole>, goals: &BTreeSet<TargetId>) -> BTreeSet<NodeId> {
    if !enabled(repo) { return BTreeSet::new(); }
    nodes.iter().filter(|n| owner(kinds.get(*n), roles.get(*n)))
        .filter(|n| incomplete(&fs::read_to_string(repo.join("Tablet").join(format!("{n}.tex")))
            .unwrap_or_default(), goals)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_citations_check_files_without_following_links() {
        let repo = tempfile::tempdir().unwrap();
        let path = "reference/model-evidence/result.md";
        let cite = format!("\\evidenceref{{{path}}}");
        assert_eq!(evidence_references(&format!("% {cite}\n{cite}\n{cite}")).unwrap(), BTreeSet::from([path.into()]));
        assert!(evidence_references("\\evidencereference").unwrap().is_empty());
        for bad in ["../file", "reference/model-evidence/../file", "reference/model-evidence/./file", "reference/model-evidence//file", "reference/internal.tex"] {
            assert!(evidence_references(&format!("\\evidenceref{{{bad}}}")).is_err());
        }
        assert!(evidence_references("\\evidenceref{unclosed").is_err());
        assert!(!reference_errors(repo.path(), "Preamble", &cite).is_empty());
        fs::create_dir_all(repo.path().join("reference/model-evidence")).unwrap();
        fs::write(repo.path().join(path), "raw record").unwrap();
        assert!(reference_errors(repo.path(), "Preamble", &cite).is_empty());
        fs::remove_file(repo.path().join(path)).unwrap();
        #[cfg(unix)] {
            fs::write(repo.path().join("outside"), "must not export").unwrap();
            std::os::unix::fs::symlink(repo.path().join("outside"), repo.path().join(path)).unwrap();
            assert!(evidence_bytes(repo.path(), path).unwrap_err().contains("symlink"));
        }
    }

    #[test]
    fn preservation_requires_each_pursued_goal_and_separates_the_paired_statement() {
        let tex = r"\begin{definition}An observation.\end{definition}
\begin{preservation}
\begin{claim}The observation retains the stated results.\end{claim}
\begin{proof}Case analysis using \noderef{Bridge}.\end{proof}
\begin{goalproof}{g1}Substitute the proved equality.\end{goalproof}
\begin{goalproof}{g2}SKETCH:
Pending.\end{goalproof}
\end{preservation}";
        let goals = BTreeSet::from([TargetId::from("g1"), TargetId::from("g2")]);
        assert!(crate::filespec::validate_tex_format(tex, false).is_empty());
        assert!(incomplete(tex, &goals));
        assert!(!incomplete(tex, &BTreeSet::from([TargetId::from("g1")])));
        assert!(incomplete("", &goals));
        assert_eq!(references(tex), BTreeSet::from(["Bridge".into()]));
        let (base, _) = split(tex).unwrap();
        assert!(!base.contains("proof"));
        assert_eq!(statements(tex), statements(&tex.replace("Pending.", "A complete application.")));
        assert!(split(&tex.replace("{g2}", "{g1}")).is_err());
    }
}
