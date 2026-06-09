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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedMainResultTargetsOutput {
    pub targets: Vec<MainResultTarget>,
    pub available_labels: Vec<String>,
    pub preview: Vec<MainResultPreviewEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TexStatementItem {
    pub id: String,
    pub env: String,
    pub title: String,
    pub body: String,
}

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

fn extract_statement_blocks_with_envs(
    paper_text: &str,
    envs: &BTreeSet<String>,
) -> Vec<PaperStatementBlock> {
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
        let env = search_text[env_start..env_end].trim().to_lowercase();
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
        let end_marker = format!("\\end{{{env}}}");
        let Some(end_rel) = search_text[content_start..].find(&end_marker) else {
            continue;
        };
        let end_start = content_start + end_rel;
        let end_end = end_start + end_marker.len();
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
        blocks.push(PaperStatementBlock {
            env,
            title,
            body,
            text: full_block.clone(),
            labels: extract_labels(&full_block),
            start_line,
            end_line,
        });
        index = end_end;
    }
    blocks
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

fn normalize_main_result_target_value(raw: &Value) -> Option<MainResultTarget> {
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

pub fn resolve_main_result_targets(
    paper_path: Option<&Path>,
    raw_targets: Option<&Value>,
    raw_labels: Option<&Value>,
) -> Result<ResolvedMainResultTargetsOutput, String> {
    let paper_text = match paper_path {
        Some(path) if path.exists() => Some(
            fs::read_to_string(path)
                .map_err(|err| format!("failed to read paper {}: {err}", path.display()))?,
        ),
        _ => None,
    };
    let default_envs: BTreeSet<String> = DEFAULT_MAIN_RESULT_ENVS
        .iter()
        .map(|env| (*env).to_string())
        .collect();
    let blocks = paper_text
        .as_deref()
        .map(|text| extract_statement_blocks_with_envs(text, &default_envs))
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
                            "Configured main_result_labels are not present as labeled paper statements: {}",
                            missing.join(", ")
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
}
