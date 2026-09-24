use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

const TEX_STATEMENT_ENVS: &[&str] = &[
    "theorem",
    "lemma",
    "definition",
    "corollary",
    "proposition",
    "helper",
];
const DEFAULT_MAIN_RESULT_ENVS: &[&str] = &["theorem", "corollary"];
const TEX_MAIN_NODE_ENVS: &[&str] = &["theorem", "lemma", "definition", "corollary", "helper"];
const TEX_PREAMBLE_ENVS: &[&str] = &["definition", "proposition"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MainResultTarget {
    pub start_line: i64,
    pub end_line: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tex_label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaperStatementBlock {
    pub env: String,
    pub title: String,
    pub body: String,
    pub text: String,
    pub labels: Vec<String>,
    pub start_line: i64,
    pub end_line: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MainResultPreviewEntry {
    pub target: MainResultTarget,
    pub env: String,
    pub text: String,
    pub start_line: i64,
    pub end_line: i64,
}

/// One cross-env nesting pair among the run's main-result environments: an
/// `outer` block whose span strictly contains an `inner` one. Widening the env
/// set is additive EXCEPT here — a newly recognized outer env matches first,
/// spans to its own `\end`, and the scan's cursor jump then swallows the inner
/// block, whose `\label` can silently rebind to the wider block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NestedMainResultBlock {
    pub outer_env: String,
    pub outer_start_line: i64,
    pub outer_end_line: i64,
    pub inner_env: String,
    pub inner_start_line: i64,
    pub inner_end_line: i64,
    pub inner_labels: Vec<String>,
    /// False when the outer block swallowed the inner one, i.e. the inner
    /// block is NOT a candidate under this env set.
    pub inner_is_candidate: bool,
    /// True when the outer candidate's binding label (its first) comes from
    /// inside the inner block — the silent label-rebinding case.
    pub rebinds_inner_label: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedMainResultTargetsOutput {
    pub targets: Vec<MainResultTarget>,
    pub available_labels: Vec<String>,
    pub preview: Vec<MainResultPreviewEntry>,
    /// Cross-env nesting among the ACTIVE env set. Reported under every env
    /// set, the default pair included (a corollary nested in a theorem is
    /// reported with no knob involved), so this is an additive wire field, NOT
    /// a byte-identical default response: resolution itself (targets,
    /// available_labels, preview) is unchanged, and consumers that read only
    /// those keys are unaffected. Omitted from the wire when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nested_blocks: Vec<NestedMainResultBlock>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TexStatementItem {
    pub id: String,
    pub env: String,
    pub title: String,
    pub body: String,
}

/// Cut every line at its first unescaped `%`, preserving line structure —
/// the comment rule the candidacy scan itself applies.
fn strip_tex_comments_preserve_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for segment in text.split_inclusive('\n') {
        let (line, newline) = match segment.strip_suffix('\n') {
            Some(line) => (line, "\n"),
            None => (segment, ""),
        };
        let bytes = line.as_bytes();
        let mut cut = bytes.len();
        let mut backslashes = 0usize;
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == b'\\' {
                backslashes += 1;
                continue;
            }
            if *byte == b'%' {
                if backslashes % 2 == 0 {
                    cut = index;
                    break;
                }
                backslashes = 0;
                continue;
            }
            backslashes = 0;
        }
        out.push_str(&line[..cut]);
        out.push_str(newline);
    }
    out
}

/// Labels appearing in a block's text, in order, deduped. The FIRST one is
/// what binds the block as a target.
fn extract_labels(block_text: &str) -> Vec<String> {
    let mut labels = Vec::new();
    let mut seen = BTreeSet::new();
    let mut search = 0usize;
    let marker = "\\label{";
    while let Some(offset) = block_text[search..].find(marker) {
        let start = search + offset + marker.len();
        if let Some(end_rel) = block_text[start..].find('}') {
            let label = block_text[start..start + end_rel].trim();
            if !label.is_empty() && seen.insert(label.to_string()) {
                labels.push(label.to_string());
            }
            search = start + end_rel + 1;
        } else {
            break;
        }
    }
    labels
}

/// The environment name of the brace group opening at `open` (the byte after a
/// `\begin{` or `\end{`), or None when what follows is not a name at all.
///
/// The bound is the point: a name is name characters — plus the padding the
/// `\begin` side has always tolerated — terminated by `}` before the line ends.
/// Searching ahead for the next `}` with no bound is what let a braceless
/// `\end{` (`\verb|\end{|` in a proof, say) consume the block's real
/// `\end{theorem}` as part of a bogus "name" and close the block on the NEXT
/// block's `\end` instead. Nothing that is not a name may cost a real close.
fn env_name_at(text: &str, open: usize) -> Option<&str> {
    let rest = &text[open..];
    for (index, ch) in rest.char_indices() {
        if ch == '}' {
            return Some(&rest[..index]);
        }
        if !(ch.is_ascii_alphanumeric() || matches!(ch, '*' | '@' | '_' | '-' | ' ' | '\t')) {
            return None;
        }
    }
    None
}

/// Where the block opened as `env_source` closes, as `(end_start, end_end)`.
///
/// LaTeX environment names are case-sensitive: `\begin{Theorem}` is closed by
/// `\end{Theorem}`, and `theorem` and `Theorem` are different environments. So
/// the closing marker is the source spelling, not the lowercased name that
/// candidacy matching uses — matching an env case-insensitively is a scanner
/// leniency, and it must not leak into where a block ENDS.
///
/// Because the two spellings ARE different environments, one can legitimately
/// nest inside the other, and the nested one's `\end` belongs to it. The walk
/// therefore tracks `\begin`s of this environment spelled differently from
/// ours and lets their `\end`s balance out. What remains — an `\end` naming
/// this environment in a spelling that closes nothing open — is a mismatched
/// pair, i.e. invalid LaTeX. Closing on it would bind a target to text LaTeX
/// delimits differently; scanning past it for a same-spelling `\end` further
/// down is how one block came to swallow the blocks after it and inherit their
/// labels. So it terminates the search with no match: the block is not
/// extracted, the cursor stays inside it, and nothing beyond that `\end` is
/// absorbed. `scripts/resolve_paper_targets.py` reports it as
/// `env_case_end_mismatch`.
///
/// A `\begin` spelled exactly like ours is NOT tracked: same-spelling nesting
/// truncates the outer block at the first `\end`, which is this scanner's
/// long-standing flat rule (see `nested_main_result_blocks`), and this fix is
/// not the place to change it.
///
/// Padding inside the braces is trimmed on both ends, matching how the
/// `\begin` side has always read a name. That leniency cannot move a boundary:
/// a padded `\end` for this environment either closes the block right there or
/// stops the walk right there, never carries it further.
fn find_block_end(
    text: &str,
    from: usize,
    env_source: &str,
    env_lower: &str,
) -> Option<(usize, usize)> {
    const BEGIN: &str = "\\begin{";
    const END: &str = "\\end{";
    let mut nested: Vec<&str> = Vec::new();
    let mut search = from;
    loop {
        let next_begin = text[search..].find(BEGIN).map(|offset| search + offset);
        let next_end = text[search..].find(END).map(|offset| search + offset);
        let (at, is_begin) = match (next_begin, next_end) {
            (Some(begin), Some(end)) => {
                if begin < end {
                    (begin, true)
                } else {
                    (end, false)
                }
            }
            (Some(begin), None) => (begin, true),
            (None, Some(end)) => (end, false),
            (None, None) => return None,
        };
        let open = at + if is_begin { BEGIN.len() } else { END.len() };
        let Some(name) = env_name_at(text, open) else {
            search = open;
            continue;
        };
        let brace = open + name.len();
        search = brace + 1;
        let name = name.trim();
        if name.to_lowercase() != env_lower {
            continue;
        }
        if is_begin {
            if name != env_source {
                nested.push(name);
            }
            continue;
        }
        if name == env_source {
            return Some((at, brace + 1));
        }
        if nested.last() == Some(&name) {
            nested.pop();
            continue;
        }
        return None;
    }
}

/// One scanned block plus its byte span in the scanned text. Containment is
/// decided on spans, never on line numbers: two blocks can share a line.
struct ScannedBlock {
    block: PaperStatementBlock,
    start: usize,
    end: usize,
}

/// Core scan. `jump_past_matched = true` is the extraction rule (a matched
/// block's `\end` moves the cursor past the whole block, so nested blocks are
/// never extracted); `false` keeps scanning inside matched blocks, which is
/// how nesting pairs are found.
fn scan_statement_blocks(
    paper_text: &str,
    envs: &BTreeSet<String>,
    jump_past_matched: bool,
) -> Vec<ScannedBlock> {
    let mut search_start = 0usize;
    let mut search_end = paper_text.len();
    if let (Some(begin), Some(end)) = (
        paper_text.find("\\begin{document}"),
        paper_text.find("\\end{document}"),
    ) {
        let begin_end = begin + "\\begin{document}".len();
        if end >= begin_end {
            search_start = begin_end;
            search_end = end;
        }
    }
    let search_text = strip_tex_comments_preserve_lines(&paper_text[search_start..search_end]);
    let line_offset = paper_text[..search_start]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as i64;
    let mut blocks = Vec::new();
    let mut index = 0usize;
    while let Some(begin_rel) = search_text[index..].find("\\begin{") {
        let begin = index + begin_rel;
        let env_start = begin + "\\begin{".len();
        let Some(env_end_rel) = search_text[env_start..].find('}') else {
            break;
        };
        let env_end = env_start + env_end_rel;
        let env_source = search_text[env_start..env_end].trim();
        let env = env_source.to_lowercase();
        index = env_end + 1;
        if !envs.contains(&env) {
            continue;
        }
        let mut content_start = env_end + 1;
        let mut title = String::new();
        if search_text[content_start..].starts_with('[') {
            if let Some(title_end_rel) = search_text[content_start + 1..].find(']') {
                title = search_text[content_start + 1..content_start + 1 + title_end_rel]
                    .trim()
                    .to_string();
                content_start = content_start + 1 + title_end_rel + 1;
            }
        }
        let Some((end_start, end_end)) =
            find_block_end(&search_text, content_start, env_source, &env)
        else {
            continue;
        };
        let full_block = search_text[begin..end_end].trim().to_string();
        let body = search_text[content_start..end_start].trim().to_string();
        let start_line = line_offset
            + search_text[..begin]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count() as i64
            + 1;
        let end_line = line_offset
            + search_text[..end_end]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count() as i64
            + 1;
        blocks.push(ScannedBlock {
            block: PaperStatementBlock {
                env,
                title,
                body,
                text: full_block.clone(),
                labels: extract_labels(&full_block),
                start_line,
                end_line,
            },
            start: begin,
            end: end_end,
        });
        if jump_past_matched {
            index = end_end;
        }
    }
    blocks
}

fn extract_statement_blocks_with_envs(
    paper_text: &str,
    envs: &BTreeSet<String>,
) -> Vec<PaperStatementBlock> {
    scan_statement_blocks(paper_text, envs, true)
        .into_iter()
        .map(|scanned| scanned.block)
        .collect()
}

/// Cross-env nesting pairs among `envs`, for callers that must warn before an
/// env-set change silently swallows a candidate (or rebinds its label).
/// Same-env nesting is excluded: it truncates the outer block at the first
/// `\end` under EVERY env set, so it is never knob-induced.
fn nested_main_result_blocks(
    paper_text: &str,
    envs: &BTreeSet<String>,
) -> Vec<NestedMainResultBlock> {
    let all = scan_statement_blocks(paper_text, envs, false);
    let extracted: BTreeSet<(usize, usize)> = scan_statement_blocks(paper_text, envs, true)
        .iter()
        .map(|scanned| (scanned.start, scanned.end))
        .collect();
    let mut nested = Vec::new();
    for outer in &all {
        for inner in &all {
            if inner.start <= outer.start || inner.end > outer.end {
                continue;
            }
            if inner.block.env == outer.block.env {
                continue;
            }
            let outer_is_candidate = extracted.contains(&(outer.start, outer.end));
            let rebinds_inner_label = outer_is_candidate
                && outer.block.labels.first().is_some_and(|binding| {
                    inner.block.labels.iter().any(|label| label == binding)
                });
            nested.push(NestedMainResultBlock {
                outer_env: outer.block.env.clone(),
                outer_start_line: outer.block.start_line,
                outer_end_line: outer.block.end_line,
                inner_env: inner.block.env.clone(),
                inner_start_line: inner.block.start_line,
                inner_end_line: inner.block.end_line,
                inner_labels: inner.block.labels.clone(),
                inner_is_candidate: extracted.contains(&(inner.start, inner.end)),
                rebinds_inner_label,
            });
        }
    }
    nested
}

pub fn extract_paper_statement_blocks(
    paper_text: &str,
    envs: Option<&BTreeSet<String>>,
) -> Vec<PaperStatementBlock> {
    let wanted_envs = envs.cloned().unwrap_or_else(|| {
        TEX_STATEMENT_ENVS
            .iter()
            .map(|env| (*env).to_string())
            .collect()
    });
    extract_statement_blocks_with_envs(paper_text, &wanted_envs)
}

/// Parse one `workflow.main_result_targets` entry (a bare label string, or an
/// object with `tex_label` and/or a line range).
pub fn normalize_main_result_target_value(raw: &Value) -> Option<MainResultTarget> {
    if let Some(label) = raw
        .as_str()
        .map(str::trim)
        .filter(|label| !label.is_empty())
    {
        return Some(MainResultTarget {
            start_line: 0,
            end_line: 0,
            tex_label: Some(label.to_string()),
        });
    }
    let obj = raw.as_object()?;
    let label = obj
        .get("tex_label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let start_line = obj.get("start_line").and_then(Value::as_i64).unwrap_or(0);
    let end_line = obj.get("end_line").and_then(Value::as_i64).unwrap_or(0);
    if start_line > 0 && end_line > 0 {
        let (start_line, end_line) = if start_line <= end_line {
            (start_line, end_line)
        } else {
            (end_line, start_line)
        };
        return Some(MainResultTarget {
            start_line,
            end_line,
            tex_label: label,
        });
    }
    label.map(|tex_label| MainResultTarget {
        start_line: 0,
        end_line: 0,
        tex_label: Some(tex_label),
    })
}

fn main_result_target_key(target: &MainResultTarget) -> String {
    if let Some(label) = target
        .tex_label
        .as_ref()
        .map(String::as_str)
        .map(str::trim)
        .filter(|label| !label.is_empty())
    {
        return format!("label:{label}");
    }
    if target.start_line > 0 && target.end_line > 0 {
        return format!("lines:{}-{}", target.start_line, target.end_line);
    }
    String::new()
}

fn infer_main_result_targets_from_blocks(blocks: &[PaperStatementBlock]) -> Vec<MainResultTarget> {
    let mut targets = Vec::new();
    let mut seen = BTreeSet::new();
    for block in blocks {
        let mut target = MainResultTarget {
            start_line: block.start_line,
            end_line: block.end_line,
            tex_label: block.labels.first().cloned(),
        };
        let key = main_result_target_key(&target);
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        if target
            .tex_label
            .as_ref()
            .is_some_and(|label| label.is_empty())
        {
            target.tex_label = None;
        }
        targets.push(target);
    }
    targets
}

fn match_block<'a>(
    target: &MainResultTarget,
    blocks: &'a [PaperStatementBlock],
) -> Option<&'a PaperStatementBlock> {
    if let Some(label) = target.tex_label.as_ref() {
        if target.start_line > 0 && target.end_line > 0 {
            if let Some(block) = blocks.iter().find(|block| {
                block.start_line == target.start_line
                    && block.end_line == target.end_line
                    && block.labels.iter().any(|existing| existing == label)
            }) {
                return Some(block);
            }
        }
        if let Some(block) = blocks
            .iter()
            .find(|block| block.labels.iter().any(|existing| existing == label))
        {
            return Some(block);
        }
    }
    if target.start_line > 0 && target.end_line > 0 {
        return blocks.iter().find(|block| {
            block.start_line == target.start_line && block.end_line == target.end_line
        });
    }
    None
}

/// Normalize (trim + lowercase, order-preserving dedupe) and validate a
/// caller-supplied main-result env set. Entries must be canonical
/// `TEX_STATEMENT_ENVS` members: verifier-side and paper-diff extraction always
/// scan the full canonical set, so a main-result env outside it would be
/// invisible to those consumers. A non-canonical author env reaches candidacy
/// through alias normalization (scripts/normalize_paper_envs.py) instead.
pub fn normalize_main_result_envs(envs: &[String]) -> Result<Vec<String>, String> {
    let mut normalized: Vec<String> = Vec::new();
    for raw in envs {
        let env = raw.trim().to_lowercase();
        if env.is_empty() {
            return Err(format!(
                "main_result_envs contains an empty environment name; allowed environments \
                 are {}",
                TEX_STATEMENT_ENVS.join(", ")
            ));
        }
        if !TEX_STATEMENT_ENVS.contains(&env.as_str()) {
            return Err(format!(
                "main_result_envs entry `{}` is not a canonical TeX statement environment \
                 (allowed: {}). Map a non-canonical environment onto a canonical one with \
                 scripts/normalize_paper_envs.py rather than widening this set.",
                raw.trim(),
                TEX_STATEMENT_ENVS.join(", ")
            ));
        }
        if !normalized.iter().any(|existing| existing == &env) {
            normalized.push(env);
        }
    }
    if normalized.is_empty() {
        return Err(format!(
            "main_result_envs is empty; omit it entirely to get the default set ({})",
            DEFAULT_MAIN_RESULT_ENVS.join(", ")
        ));
    }
    Ok(normalized)
}

/// `main_result_envs = None` ⇒ `DEFAULT_MAIN_RESULT_ENVS`, byte-identically to
/// the pre-knob resolver.
pub fn resolve_main_result_targets(
    paper_path: Option<&Path>,
    raw_targets: Option<&Value>,
    raw_labels: Option<&Value>,
    main_result_envs: Option<&[String]>,
) -> Result<ResolvedMainResultTargetsOutput, String> {
    let paper_text = match paper_path {
        Some(path) if path.exists() => Some(
            fs::read_to_string(path)
                .map_err(|err| format!("failed to read paper {}: {err}", path.display()))?,
        ),
        _ => None,
    };
    let env_names: Vec<String> = match main_result_envs {
        Some(envs) => normalize_main_result_envs(envs)?,
        None => DEFAULT_MAIN_RESULT_ENVS
            .iter()
            .map(|env| (*env).to_string())
            .collect(),
    };
    let envs: BTreeSet<String> = env_names.iter().cloned().collect();
    let blocks = paper_text
        .as_deref()
        .map(|text| extract_statement_blocks_with_envs(text, &envs))
        .unwrap_or_default();
    let nested_blocks = paper_text
        .as_deref()
        .map(|text| nested_main_result_blocks(text, &envs))
        .unwrap_or_default();
    let available_labels: Vec<String> = blocks
        .iter()
        .flat_map(|block| block.labels.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let label_index: BTreeMap<String, MainResultTarget> = blocks
        .iter()
        .filter_map(|block| {
            block.labels.first().map(|label| {
                (
                    label.clone(),
                    MainResultTarget {
                        start_line: block.start_line,
                        end_line: block.end_line,
                        tex_label: Some(label.clone()),
                    },
                )
            })
        })
        .collect();

    let mut resolved = Vec::new();
    let mut seen = BTreeSet::new();
    let mut add_target = |raw: &Value| {
        let Some(mut target) = normalize_main_result_target_value(raw) else {
            return;
        };
        if let Some(label) = target.tex_label.as_ref() {
            if let Some(enriched) = label_index.get(label) {
                target = enriched.clone();
            }
        }
        let key = main_result_target_key(&target);
        if !key.is_empty() && seen.insert(key) {
            resolved.push(target);
        }
    };

    match raw_targets {
        Some(Value::Array(items)) if !items.is_empty() => {
            for raw in items {
                add_target(raw);
            }
        }
        _ => match raw_labels {
            Some(Value::Array(labels)) if !labels.is_empty() => {
                let requested: Vec<String> = labels
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|label| !label.is_empty())
                    .map(str::to_string)
                    .collect();
                if paper_text.is_some() {
                    let missing: Vec<String> = requested
                        .iter()
                        .filter(|label| !available_labels.iter().any(|known| known == *label))
                        .cloned()
                        .collect();
                    if !missing.is_empty() {
                        return Err(format!(
                            "Configured main_result_labels are not present as labeled paper \
                             statements: {} (main-result resolution scans {} environments only; \
                             a label on e.g. a lemma block cannot resolve here)",
                            missing.join(", "),
                            env_names.join("/")
                        ));
                    }
                }
                for label in requested {
                    add_target(&Value::String(label));
                }
            }
            _ => {
                resolved = infer_main_result_targets_from_blocks(&blocks);
            }
        },
    }

    let mut preview = Vec::new();
    if paper_text.is_some() {
        for target in &resolved {
            let Some(block) = match_block(target, &blocks) else {
                let label_text = target
                    .tex_label
                    .clone()
                    .unwrap_or_else(|| format!("lines {}-{}", target.start_line, target.end_line));
                return Err(format!(
                    "Could not locate paper text for resolved main-result target {label_text}."
                ));
            };
            preview.push(MainResultPreviewEntry {
                target: target.clone(),
                env: block.env.clone(),
                text: block.text.clone(),
                start_line: block.start_line,
                end_line: block.end_line,
            });
        }
    }

    Ok(ResolvedMainResultTargetsOutput {
        targets: resolved,
        available_labels,
        preview,
        nested_blocks,
    })
}

pub fn extract_tex_statement_items(tex_content: &str, is_preamble: bool) -> Vec<TexStatementItem> {
    let allowed: BTreeSet<String> = if is_preamble {
        TEX_PREAMBLE_ENVS
            .iter()
            .map(|env| (*env).to_string())
            .collect()
    } else {
        TEX_MAIN_NODE_ENVS
            .iter()
            .map(|env| (*env).to_string())
            .collect()
    };
    extract_statement_blocks_with_envs(tex_content, &allowed)
        .into_iter()
        .enumerate()
        .map(|(index, block)| TexStatementItem {
            id: if is_preamble {
                format!("Preamble[{}]", index + 1)
            } else {
                format!("Item[{}]", index + 1)
            },
            env: block.env,
            title: block.title,
            body: block.body,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_targets_enriches_labels_from_paper() {
        let paper = r#"
\begin{document}
\begin{theorem}\label{thm:conn}
Statement.
\end{theorem}
\end{document}
"#;
        let tmp = tempfile::tempdir().expect("tempdir");
        let paper_path = tmp.path().join("paper.tex");
        fs::write(&paper_path, paper).expect("write paper");
        let output = resolve_main_result_targets(
            Some(&paper_path),
            None,
            Some(&serde_json::json!(["thm:conn"])),
            None,
        )
        .expect("resolve targets");
        assert_eq!(
            output.targets,
            vec![MainResultTarget {
                start_line: 3,
                end_line: 5,
                tex_label: Some("thm:conn".into()),
            }]
        );
        assert_eq!(output.preview.len(), 1);
        assert_eq!(output.preview[0].env, "theorem");
    }

    fn write_paper(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paper_path = tmp.path().join("paper.tex");
        fs::write(&paper_path, text).expect("write paper");
        (tmp, paper_path)
    }

    const MIXED_ENV_PAPER: &str = r#"
\begin{document}
\begin{theorem}\label{thm:main}
Main.
\end{theorem}
\begin{lemma}\label{lem:key}
Key.
\end{lemma}
\begin{corollary}\label{cor:one}
Cor.
\end{corollary}
\end{document}
"#;

    #[test]
    fn absent_env_list_equals_the_default_pair() {
        let (_tmp, paper_path) = write_paper(MIXED_ENV_PAPER);
        let default_output = resolve_main_result_targets(Some(&paper_path), None, None, None)
            .expect("resolve with default envs");
        let explicit_output = resolve_main_result_targets(
            Some(&paper_path),
            None,
            None,
            Some(&["theorem".to_string(), "corollary".to_string()]),
        )
        .expect("resolve with the default pair spelled out");
        assert_eq!(default_output, explicit_output);
        assert_eq!(
            default_output.available_labels,
            vec!["cor:one".to_string(), "thm:main".to_string()]
        );

        // Equivalence must hold on a paper that nests within the DEFAULT pair
        // too — the case where the response does carry a nesting report.
        let (_nested_tmp, nested_paper) = write_paper(
            r#"
\begin{document}
\begin{theorem}\label{thm:outer}
Outer.
\begin{corollary}\label{cor:inner}
Inner.
\end{corollary}
\end{theorem}
\end{document}
"#,
        );
        let nested_default = resolve_main_result_targets(Some(&nested_paper), None, None, None)
            .expect("resolve with default envs");
        let nested_explicit = resolve_main_result_targets(
            Some(&nested_paper),
            None,
            None,
            Some(&["theorem".to_string(), "corollary".to_string()]),
        )
        .expect("resolve with the default pair spelled out");
        assert_eq!(nested_default, nested_explicit);
        assert!(
            !nested_default.nested_blocks.is_empty(),
            "the default pair reports nesting; the response is additive, not byte-identical"
        );
    }

    #[test]
    fn explicit_env_list_widens_candidacy() {
        let (_tmp, paper_path) = write_paper(MIXED_ENV_PAPER);
        let output = resolve_main_result_targets(
            Some(&paper_path),
            None,
            None,
            // Trimmed + lowercased before matching, like the env names in the paper.
            Some(&[" Theorem ".to_string(), "LEMMA".to_string()]),
        )
        .expect("resolve with a widened env set");
        assert_eq!(
            output.available_labels,
            vec!["lem:key".to_string(), "thm:main".to_string()]
        );
        let labels: Vec<Option<String>> = output
            .targets
            .iter()
            .map(|target| target.tex_label.clone())
            .collect();
        assert_eq!(
            labels,
            vec![Some("thm:main".to_string()), Some("lem:key".to_string())]
        );
    }

    #[test]
    fn env_list_must_be_a_subset_of_the_canonical_statement_envs() {
        let (_tmp, paper_path) = write_paper(MIXED_ENV_PAPER);
        let err = resolve_main_result_targets(
            Some(&paper_path),
            None,
            None,
            Some(&["theorem".to_string(), "conjecture".to_string()]),
        )
        .expect_err("non-canonical env must be rejected");
        assert!(err.contains("`conjecture`"), "{err}");
        assert!(err.contains("normalize_paper_envs.py"), "{err}");

        let empty_err = resolve_main_result_targets(Some(&paper_path), None, None, Some(&[]))
            .expect_err("an empty env set must be rejected");
        assert!(empty_err.contains("main_result_envs is empty"), "{empty_err}");
    }

    #[test]
    fn labels_only_error_names_the_configured_env_set() {
        let (_tmp, paper_path) = write_paper(MIXED_ENV_PAPER);
        let err = resolve_main_result_targets(
            Some(&paper_path),
            None,
            Some(&serde_json::json!(["cor:one"])),
            Some(&["theorem".to_string(), "lemma".to_string()]),
        )
        .expect_err("a corollary label is out of set here");
        assert!(err.contains("scans theorem/lemma environments only"), "{err}");
    }

    /// Nesting direction 1: a newly recognized env INSIDE an already
    /// recognized block. Additive — the theorem's boundaries are found by its
    /// own `\end`, and the nested proposition stays unextracted.
    #[test]
    fn new_env_nested_inside_a_candidate_is_additive() {
        let (_tmp, paper_path) = write_paper(
            r#"
\begin{document}
\begin{theorem}\label{thm:outer}
Outer.
\begin{proposition}\label{prop:inner}
Inner.
\end{proposition}
\end{theorem}
\end{document}
"#,
        );
        let before = resolve_main_result_targets(Some(&paper_path), None, None, None)
            .expect("resolve with default envs");
        let after = resolve_main_result_targets(
            Some(&paper_path),
            None,
            None,
            Some(&[
                "theorem".to_string(),
                "corollary".to_string(),
                "proposition".to_string(),
            ]),
        )
        .expect("resolve with the widened env set");
        assert_eq!(before.targets, after.targets);
        assert_eq!(
            after.targets,
            vec![MainResultTarget {
                start_line: 3,
                end_line: 8,
                tex_label: Some("thm:outer".into()),
            }]
        );
        assert!(before.nested_blocks.is_empty());
        assert_eq!(after.nested_blocks.len(), 1);
        let nested = &after.nested_blocks[0];
        assert_eq!(nested.outer_env, "theorem");
        assert_eq!((nested.outer_start_line, nested.outer_end_line), (3, 8));
        assert_eq!(nested.inner_env, "proposition");
        assert_eq!((nested.inner_start_line, nested.inner_end_line), (5, 7));
        assert!(!nested.inner_is_candidate);
        assert!(!nested.rebinds_inner_label);
    }

    /// Nesting direction 2: an already recognized env INSIDE a newly
    /// recognized one. NOT additive — the proposition matches first, the
    /// cursor jump swallows the theorem, and the theorem's label rebinds to
    /// the wider block.
    #[test]
    fn widened_env_swallows_a_nested_candidate_and_rebinds_its_label() {
        let (_tmp, paper_path) = write_paper(
            r#"
\begin{document}
\begin{proposition}
Outer.
\begin{theorem}\label{thm:inner}
Inner.
\end{theorem}
\end{proposition}
\end{document}
"#,
        );
        let before = resolve_main_result_targets(Some(&paper_path), None, None, None)
            .expect("resolve with default envs");
        assert_eq!(
            before.targets,
            vec![MainResultTarget {
                start_line: 5,
                end_line: 7,
                tex_label: Some("thm:inner".into()),
            }]
        );
        assert!(before.nested_blocks.is_empty());

        let after = resolve_main_result_targets(
            Some(&paper_path),
            None,
            None,
            Some(&[
                "theorem".to_string(),
                "corollary".to_string(),
                "proposition".to_string(),
            ]),
        )
        .expect("resolve with the widened env set");
        assert_eq!(
            after.targets,
            vec![MainResultTarget {
                start_line: 3,
                end_line: 8,
                tex_label: Some("thm:inner".into()),
            }],
            "same label, wider block"
        );
        assert_eq!(after.preview[0].env, "proposition");
        assert_eq!(after.nested_blocks.len(), 1);
        let nested = &after.nested_blocks[0];
        assert_eq!(nested.outer_env, "proposition");
        assert_eq!((nested.outer_start_line, nested.outer_end_line), (3, 8));
        assert_eq!(nested.inner_env, "theorem");
        assert_eq!((nested.inner_start_line, nested.inner_end_line), (5, 7));
        assert_eq!(nested.inner_labels, vec!["thm:inner".to_string()]);
        assert!(!nested.inner_is_candidate);
        assert!(nested.rebinds_inner_label);
    }

    /// Nesting is a property of the ACTIVE env set, not of the knob: the
    /// default pair alone nests when a corollary sits inside a theorem. The
    /// resolution itself is untouched — only the additive report appears.
    #[test]
    fn default_env_set_reports_cross_env_nesting_too() {
        let (_tmp, paper_path) = write_paper(
            r#"
\begin{document}
\begin{theorem}\label{thm:outer}
Outer.
\begin{corollary}\label{cor:inner}
Inner.
\end{corollary}
\end{theorem}
\end{document}
"#,
        );
        let output = resolve_main_result_targets(Some(&paper_path), None, None, None)
            .expect("resolve with default envs");
        assert_eq!(
            output.targets,
            vec![MainResultTarget {
                start_line: 3,
                end_line: 8,
                tex_label: Some("thm:outer".into()),
            }]
        );
        assert_eq!(output.nested_blocks.len(), 1, "no knob involved");
        assert_eq!(output.nested_blocks[0].outer_env, "theorem");
        assert_eq!(output.nested_blocks[0].inner_env, "corollary");
        assert!(!output.nested_blocks[0].inner_is_candidate);
    }

    /// A capitalized environment is matched case-insensitively and closed by
    /// its OWN `\end`, the way LaTeX closes it.
    #[test]
    fn capitalized_env_is_opened_and_closed_by_its_own_end() {
        let (_tmp, paper_path) = write_paper(
            r#"
\begin{document}
\begin{Theorem}\label{thm:upper}
Upper.
\end{Theorem}
\end{document}
"#,
        );
        let output = resolve_main_result_targets(Some(&paper_path), None, None, None)
            .expect("resolve with default envs");
        assert_eq!(
            output.targets,
            vec![MainResultTarget {
                start_line: 3,
                end_line: 5,
                tex_label: Some("thm:upper".into()),
            }],
            "the block must not be dropped for spelling its env in mixed case"
        );
        assert_eq!(output.preview[0].env, "theorem");
        assert_eq!(output.available_labels, vec!["thm:upper".to_string()]);
    }

    /// The swallow: a capitalized block used to run to the next LOWERCASE
    /// `\end`, absorbing every block in between and inheriting its label.
    #[test]
    fn capitalized_env_does_not_swallow_the_blocks_after_it() {
        let (_tmp, paper_path) = write_paper(
            r#"
\begin{document}
\begin{Theorem}\label{thm:upper}
Upper.
\end{Theorem}
\begin{theorem}\label{thm:lower}
Lower.
\end{theorem}
\end{document}
"#,
        );
        let output = resolve_main_result_targets(Some(&paper_path), None, None, None)
            .expect("resolve with default envs");
        assert_eq!(
            output.targets,
            vec![
                MainResultTarget {
                    start_line: 3,
                    end_line: 5,
                    tex_label: Some("thm:upper".into()),
                },
                MainResultTarget {
                    start_line: 6,
                    end_line: 8,
                    tex_label: Some("thm:lower".into()),
                },
            ],
            "two separate blocks, each closed by its own \\end"
        );
        assert_eq!(
            output.available_labels,
            vec!["thm:lower".to_string(), "thm:upper".to_string()]
        );
        assert!(output.nested_blocks.is_empty(), "neither block contains the other");
    }

    /// A genuinely mismatched pair is invalid LaTeX. It costs its own block —
    /// never the blocks after it, and never their labels.
    #[test]
    fn mismatched_case_pair_drops_only_its_own_block() {
        let (_tmp, paper_path) = write_paper(
            r#"
\begin{document}
\begin{Theorem}\label{thm:mismatch}
Opened upper, closed lower.
\end{theorem}
\begin{theorem}\label{thm:ok}
Well formed.
\end{theorem}
\end{document}
"#,
        );
        let output = resolve_main_result_targets(Some(&paper_path), None, None, None)
            .expect("resolve with default envs");
        assert_eq!(
            output.targets,
            vec![MainResultTarget {
                start_line: 6,
                end_line: 8,
                tex_label: Some("thm:ok".into()),
            }]
        );
        assert!(
            !output.available_labels.contains(&"thm:mismatch".to_string()),
            "the unclosed block binds nothing: {:?}",
            output.available_labels
        );

        // Symmetrically, a lowercase block is not closed by a capitalized
        // `\end` either — and does not run past it to the next lowercase one.
        let (_flip_tmp, flipped) = write_paper(
            r#"
\begin{document}
\begin{theorem}\label{thm:flipped}
Opened lower, closed upper.
\end{Theorem}
\begin{theorem}\label{thm:after}
After.
\end{theorem}
\end{document}
"#,
        );
        let flipped_output = resolve_main_result_targets(Some(&flipped), None, None, None)
            .expect("resolve with default envs");
        assert_eq!(
            flipped_output.targets,
            vec![MainResultTarget {
                start_line: 6,
                end_line: 8,
                tex_label: Some("thm:after".into()),
            }]
        );
    }

    /// Case-insensitive candidacy still means one environment: a capitalized
    /// block is closed by its own `\end`, so a differently-cased block nested
    /// in it is swallowed exactly as a same-case one would be.
    #[test]
    fn end_search_ignores_ends_of_other_environments() {
        let (_tmp, paper_path) = write_paper(
            r#"
\begin{document}
\begin{Theorem}\label{thm:outer}
Outer.
\begin{lemma}\label{lem:inner}
Inner.
\end{lemma}
\end{Theorem}
\end{document}
"#,
        );
        let output = resolve_main_result_targets(Some(&paper_path), None, None, None)
            .expect("resolve with default envs");
        assert_eq!(
            output.targets,
            vec![MainResultTarget {
                start_line: 3,
                end_line: 8,
                tex_label: Some("thm:outer".into()),
            }],
            "the lemma's \\end must not close the theorem"
        );
    }

    /// A braceless `\end{` — quoted LaTeX in a proof — is not an environment
    /// name. The name scan must stop at it instead of running to the next `}`
    /// in the document, which is the block's OWN `\end{theorem}`: consuming
    /// that as part of a bogus name closes the block on the next block's
    /// `\end` and swallows it.
    #[test]
    fn braceless_end_marker_cannot_consume_a_real_close() {
        let (_tmp, paper_path) = write_paper(
            r#"
\begin{document}
\begin{theorem}\label{thm:verb}
We write \verb|\end{| for the marker.
\end{theorem}
\begin{theorem}\label{thm:next}
Next.
\end{theorem}
\end{document}
"#,
        );
        let output = resolve_main_result_targets(Some(&paper_path), None, None, None)
            .expect("resolve with default envs");
        assert_eq!(
            output.targets,
            vec![
                MainResultTarget {
                    start_line: 3,
                    end_line: 5,
                    tex_label: Some("thm:verb".into()),
                },
                MainResultTarget {
                    start_line: 6,
                    end_line: 8,
                    tex_label: Some("thm:next".into()),
                },
            ]
        );
        assert!(
            !output.preview[0].text.contains("thm:next"),
            "the first block must not absorb the second: {:?}",
            output.preview[0].text
        );
    }

    /// `Theorem` and `theorem` are different LaTeX environments, so one can
    /// legitimately nest inside the other and the inner `\end` belongs to the
    /// inner block. The outer block resolves; it is not dropped as a case
    /// mismatch.
    #[test]
    fn differently_cased_nesting_is_balanced_not_a_mismatch() {
        let (_tmp, paper_path) = write_paper(
            r#"
\begin{document}
\begin{Theorem}\label{thm:outer}
Outer.
\begin{theorem}\label{thm:inner}
Inner.
\end{theorem}
\end{Theorem}
\end{document}
"#,
        );
        let output = resolve_main_result_targets(Some(&paper_path), None, None, None)
            .expect("resolve with default envs");
        assert_eq!(
            output.targets,
            vec![MainResultTarget {
                start_line: 3,
                end_line: 8,
                tex_label: Some("thm:outer".into()),
            }],
            "the inner \\end{{theorem}} closes the inner block, not the outer one"
        );
        assert!(output.available_labels.contains(&"thm:outer".to_string()));
    }

    /// The other side of that balance: an `\end` for this environment in a
    /// spelling that closes nothing open is a mismatched pair. It stops the
    /// walk, and the blocks beyond it are untouched.
    #[test]
    fn unbalanced_differently_cased_end_absorbs_nothing_beyond_itself() {
        let (_tmp, paper_path) = write_paper(
            r#"
\begin{document}
\begin{Theorem}\label{thm:outer}
\begin{theorem}\label{thm:inner}
Inner.
\end{theorem}
\end{theorem}
\begin{theorem}\label{thm:later}
Later.
\end{theorem}
\end{document}
"#,
        );
        let output = resolve_main_result_targets(Some(&paper_path), None, None, None)
            .expect("resolve with default envs");
        assert_eq!(
            output.targets,
            vec![
                MainResultTarget {
                    start_line: 4,
                    end_line: 6,
                    tex_label: Some("thm:inner".into()),
                },
                MainResultTarget {
                    start_line: 8,
                    end_line: 10,
                    tex_label: Some("thm:later".into()),
                },
            ],
            "the outer block stops at the unbalanced \\end and costs only itself"
        );
        assert!(!output.available_labels.contains(&"thm:outer".to_string()));
    }

    /// Padding inside the braces reads the same on both ends. The `\begin`
    /// side has always trimmed, so a padded `\begin` still resolves; and a
    /// padded `\end` closes the block where the author wrote it rather than
    /// being skipped in favour of a later same-spelling `\end`, which is how a
    /// one-sided trim swallowed the block in between.
    #[test]
    fn padding_inside_the_braces_reads_the_same_on_both_ends() {
        let (_tmp, padded_begin) = write_paper(
            r#"
\begin{document}
\begin{ theorem }\label{thm:padded}
Padded open.
\end{theorem}
\end{document}
"#,
        );
        let output = resolve_main_result_targets(Some(&padded_begin), None, None, None)
            .expect("resolve with default envs");
        assert_eq!(
            output.targets,
            vec![MainResultTarget {
                start_line: 3,
                end_line: 5,
                tex_label: Some("thm:padded".into()),
            }]
        );

        let (_tmp, padded_end) = write_paper(
            r#"
\begin{document}
\begin{theorem}\label{thm:first}
First.
\end{ theorem }
\begin{theorem}\label{thm:second}
Second.
\end{theorem}
\end{document}
"#,
        );
        let output = resolve_main_result_targets(Some(&padded_end), None, None, None)
            .expect("resolve with default envs");
        assert_eq!(
            output.targets,
            vec![
                MainResultTarget {
                    start_line: 3,
                    end_line: 5,
                    tex_label: Some("thm:first".into()),
                },
                MainResultTarget {
                    start_line: 6,
                    end_line: 8,
                    tex_label: Some("thm:second".into()),
                },
            ],
            "a padded \\end closes its own block instead of ceding it to the next one"
        );
    }

    #[test]
    fn same_env_nesting_is_not_reported_as_knob_induced() {
        let (_tmp, paper_path) = write_paper(
            r#"
\begin{document}
\begin{theorem}\label{thm:outer}
Outer.
\begin{theorem}\label{thm:inner}
Inner.
\end{theorem}
\end{theorem}
\end{document}
"#,
        );
        let output = resolve_main_result_targets(Some(&paper_path), None, None, None)
            .expect("resolve with default envs");
        assert!(output.nested_blocks.is_empty());
    }
}
