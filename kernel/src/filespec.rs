use std::collections::BTreeSet;

pub const MAIN_NODE_ENVS: &[&str] = &["theorem", "lemma", "definition", "corollary", "helper"];
pub const PREAMBLE_ENVS: &[&str] = &["definition", "proposition"];
pub const PROOF_BEARING_ENVS: &[&str] = &["theorem", "lemma", "corollary", "helper"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationHead {
    pub kind: String,
    pub name: String,
    pub line: u32,
}

fn parse_decl_line(trimmed: &str) -> Option<(String, String)> {
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    if tokens[0] == "noncomputable"
        && tokens.len() >= 3
        && (tokens[1] == "def" || tokens[1] == "theorem")
    {
        return Some((
            format!("{} {}", tokens[0], tokens[1]),
            tokens[2].to_string(),
        ));
    }
    if ["theorem", "lemma", "def", "abbrev", "example"].contains(&tokens[0]) && tokens.len() >= 2 {
        return Some((tokens[0].to_string(), tokens[1].to_string()));
    }
    None
}

pub fn declaration_heads(lean_content: &str) -> Vec<DeclarationHead> {
    let mut heads = Vec::new();
    for (idx, line) in lean_content.lines().enumerate() {
        if let Some((kind, name)) = parse_decl_line(line.trim()) {
            heads.push(DeclarationHead {
                kind,
                name,
                line: (idx + 1) as u32,
            });
        }
    }
    heads
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopLevelEnvParse {
    pub envs: Vec<String>,
    pub errors: Vec<String>,
}

fn parse_braced_name(content: &str, start: usize, prefix: &str) -> Option<(String, usize)> {
    let rest = content.get(start..)?;
    let after_prefix = rest.strip_prefix(prefix)?;
    let end = after_prefix.find('}')?;
    Some((
        after_prefix[..end].trim().to_ascii_lowercase(),
        start + prefix.len() + end + 1,
    ))
}

pub fn parse_top_level_envs(tex_content: &str) -> TopLevelEnvParse {
    let mut envs = Vec::new();
    let mut errors = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    let begin_prefix = "\\begin{";
    let end_prefix = "\\end{";

    loop {
        let begin_rel = tex_content[cursor..].find(begin_prefix);
        let end_rel = tex_content[cursor..].find(end_prefix);
        let next = match (begin_rel, end_rel) {
            (None, None) => {
                if stack.is_empty() && !tex_content[cursor..].trim().is_empty() {
                    errors.push(
                        "Non-whitespace text is not allowed outside top-level environments"
                            .to_string(),
                    );
                }
                break;
            }
            (Some(b), None) => ("begin", cursor + b),
            (None, Some(e)) => ("end", cursor + e),
            (Some(b), Some(e)) => {
                if b <= e {
                    ("begin", cursor + b)
                } else {
                    ("end", cursor + e)
                }
            }
        };

        let token_start = next.1;
        if stack.is_empty() && !tex_content[cursor..token_start].trim().is_empty() {
            errors.push(
                "Non-whitespace text is not allowed outside top-level environments".to_string(),
            );
        }

        match next.0 {
            "begin" => {
                let Some((env, next_cursor)) =
                    parse_braced_name(tex_content, token_start, begin_prefix)
                else {
                    errors.push("Malformed \\begin{...} block".to_string());
                    break;
                };
                if stack.is_empty() {
                    envs.push(env.clone());
                }
                stack.push(env);
                cursor = next_cursor;
            }
            "end" => {
                let Some((env, next_cursor)) =
                    parse_braced_name(tex_content, token_start, end_prefix)
                else {
                    errors.push("Malformed \\end{...} block".to_string());
                    break;
                };
                let Some(open) = stack.pop() else {
                    errors.push(format!("Unexpected top-level \\end{{{env}}}"));
                    cursor = next_cursor;
                    continue;
                };
                if open != env {
                    errors.push(format!(
                        "Mismatched environment nesting: opened {open}, closed {env}"
                    ));
                }
                cursor = next_cursor;
            }
            _ => unreachable!(),
        }
    }

    if !stack.is_empty() {
        errors.push(format!(
            "Unclosed environment(s): {}",
            stack.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }

    TopLevelEnvParse { envs, errors }
}

pub fn tex_statement_environment(tex_content: &str) -> String {
    parse_top_level_envs(tex_content)
        .envs
        .into_iter()
        .find(|env| MAIN_NODE_ENVS.contains(&env.as_str()) || PREAMBLE_ENVS.contains(&env.as_str()))
        .unwrap_or_default()
}

pub fn validate_tex_format(tex_content: &str, is_preamble: bool) -> Vec<String> {
    let parsed = parse_top_level_envs(tex_content);
    let mut errors = parsed.errors;
    let envs = parsed.envs;

    if is_preamble {
        for env in envs {
            if !PREAMBLE_ENVS.contains(&env.as_str()) {
                errors.push(format!(
                    "Preamble .tex top-level environments must be definition/proposition only, found {env}"
                ));
            }
        }
        return errors;
    }

    match envs.as_slice() {
        [env] if *env == "definition" => {}
        [env, proof] if PROOF_BEARING_ENVS.contains(&env.as_str()) && *proof == "proof" => {}
        [] => errors.push(
            "Ordinary tablet node .tex must contain either a single definition block or a theorem-like block followed by a proof block".to_string(),
        ),
        _ => errors.push(format!(
            "Ordinary tablet node .tex has invalid top-level block sequence {:?}; expected [definition] or [theorem|lemma|corollary|helper, proof]",
            envs
        )),
    }

    errors
}

pub fn validate_lean_node_shape(lean_content: &str, node_name: &str) -> Vec<String> {
    let heads = declaration_heads(lean_content);
    let matching: Vec<&DeclarationHead> =
        heads.iter().filter(|head| head.name == node_name).collect();
    let mut errors = Vec::new();
    if matching.is_empty() {
        errors.push(format!(
            "Lean node file must contain a top-level declaration named {node_name}"
        ));
    } else if matching.len() > 1 {
        let lines: Vec<String> = matching.iter().map(|head| head.line.to_string()).collect();
        errors.push(format!(
            "Lean node file must not contain multiple top-level declarations named {node_name}; found at lines {}",
            lines.join(", ")
        ));
    }
    let extra_named: BTreeSet<String> = heads
        .iter()
        .filter(|head| head.name != node_name)
        .map(|head| head.name.clone())
        .collect();
    if !extra_named.is_empty() {
        errors.push(format!(
            "Lean node file should have a single principal top-level declaration matching the node; found additional declarations {:?}",
            extra_named
        ));
    }
    errors
}

pub fn is_proof_bearing_statement_environment(env: &str) -> bool {
    PROOF_BEARING_ENVS.contains(&env)
}

/// Auto-fix policy: every tablet node should transitively `import Tablet.Preamble`,
/// either directly or via another tablet node's import. When a node has neither
/// a direct Preamble import nor any other tablet import, we inject
/// `import Tablet.Preamble` at the top of the imports block. Idempotent.
///
/// Returns `Some(modified)` if a Preamble import was added, `None` if no change
/// was needed (file already imports Preamble OR imports another tablet node).
///
/// The Preamble file itself is exempt — it is the import root.
pub fn ensure_preamble_import_for_orphan(content: &str, node_name: &str) -> Option<String> {
    if node_name == "Preamble" {
        return None;
    }
    let imports: Vec<String> = content
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix("import ")
                .map(str::trim)
                .map(str::to_string)
        })
        .filter(|s| !s.is_empty())
        .collect();
    let preamble_present = imports.iter().any(|i| i == "Tablet.Preamble");
    if preamble_present {
        return None;
    }
    let other_tablet_imports = imports
        .iter()
        .any(|i| i.starts_with("Tablet.") && i != "Tablet.Preamble");
    if other_tablet_imports {
        return None;
    }

    // Insert `import Tablet.Preamble` at the right place: after the last
    // existing `import` line, or at the very top if none.
    let lines: Vec<&str> = content.lines().collect();
    let mut last_import_idx: Option<usize> = None;
    for (idx, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("import ") {
            last_import_idx = Some(idx);
        }
    }
    let insert_at = match last_import_idx {
        Some(idx) => idx + 1,
        None => 0,
    };
    let mut new_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    new_lines.insert(insert_at, "import Tablet.Preamble".to_string());
    let mut result = new_lines.join("\n");
    if content.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }
    Some(result)
}

/// Apply [`ensure_preamble_import_for_orphan`] to a node's `.lean` file on
/// disk under `<repo_path>/Tablet/<node>.lean`. Returns `Ok(true)` if the
/// file was rewritten, `Ok(false)` if no change was needed or the file
/// doesn't exist. Best-effort: I/O errors propagate.
pub fn normalize_node_lean_imports_on_disk(
    repo_path: &std::path::Path,
    node: &str,
) -> std::io::Result<bool> {
    let path = repo_path.join("Tablet").join(format!("{node}.lean"));
    if !path.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(&path)?;
    if let Some(new_content) = ensure_preamble_import_for_orphan(&content, node) {
        std::fs::write(&path, new_content)?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::{
        declaration_heads, parse_top_level_envs, tex_statement_environment,
        validate_lean_node_shape, validate_tex_format,
    };

    #[test]
    fn ordinary_tex_requires_exact_top_level_shapes() {
        assert!(validate_tex_format("\\begin{definition}x\\end{definition}\n", false).is_empty());
        assert!(validate_tex_format(
            "\\begin{lemma}x\\end{lemma}\n\\begin{proof}y\\end{proof}\n",
            false
        )
        .is_empty());
        assert!(!validate_tex_format(
            "intro\n\\begin{lemma}x\\end{lemma}\n\\begin{proof}y\\end{proof}\n",
            false
        )
        .is_empty());
        assert!(!validate_tex_format("\\begin{lemma}x\\end{lemma}\n", false).is_empty());
        assert!(!validate_tex_format(
            "\\begin{lemma}x\\end{lemma}\n\\begin{proof}y\\end{proof}\n\\begin{lemma}z\\end{lemma}",
            false
        )
        .is_empty());
    }

    #[test]
    fn preamble_tex_disallows_free_text_and_non_definition_blocks() {
        assert!(validate_tex_format("", true).is_empty());
        assert!(validate_tex_format(
            "\\begin{definition}x\\end{definition}\n\\begin{proposition}y\\end{proposition}\n",
            true
        )
        .is_empty());
        assert!(!validate_tex_format("\\newcommand{\\PP}{x}", true).is_empty());
        assert!(!validate_tex_format("\\begin{lemma}x\\end{lemma}", true).is_empty());
    }

    #[test]
    fn top_level_env_parser_ignores_nested_envs_inside_blocks() {
        let parsed = parse_top_level_envs(
            "\\begin{theorem}a\\begin{enumerate}\\item x\\end{enumerate}\\end{theorem}\\begin{proof}b\\end{proof}",
        );
        assert!(parsed.errors.is_empty());
        assert_eq!(
            parsed.envs,
            vec!["theorem".to_string(), "proof".to_string()]
        );
        assert_eq!(
            tex_statement_environment("\\begin{helper}a\\end{helper}\\begin{proof}b\\end{proof}"),
            "helper"
        );
    }

    #[test]
    fn lean_node_shape_prefers_single_principal_declaration() {
        assert!(validate_lean_node_shape("-- [TABLET NODE: Foo]\nimport Tablet.Preamble\n\ntheorem Foo : True := by\n  trivial\n", "Foo").is_empty());
        assert!(!validate_lean_node_shape(
            "theorem Foo : True := by\n  trivial\n\ntheorem Bar : True := by\n  trivial\n",
            "Foo"
        )
        .is_empty());
        let heads = declaration_heads("def Foo := 1\nlemma Bar : True := by trivial\n");
        assert_eq!(heads.len(), 2);
    }

    #[test]
    fn ensure_preamble_import_for_orphan_adds_when_no_tablet_imports() {
        let src = "import Mathlib.Topology.Basic\n\n-- [TABLET NODE: Foo]\ntheorem Foo : True := by trivial\n";
        let out = super::ensure_preamble_import_for_orphan(src, "Foo").expect("should rewrite");
        assert!(out.contains("import Tablet.Preamble"));
        // Inserted after the existing import line, before the marker comment.
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "import Mathlib.Topology.Basic");
        assert_eq!(lines[1], "import Tablet.Preamble");
        assert_eq!(out.ends_with('\n'), true);
    }

    #[test]
    fn ensure_preamble_import_for_orphan_skips_when_preamble_already_present() {
        let src = "import Tablet.Preamble\nimport Mathlib.Topology.Basic\n\ntheorem Foo : True := by trivial\n";
        assert!(super::ensure_preamble_import_for_orphan(src, "Foo").is_none());
    }

    #[test]
    fn ensure_preamble_import_for_orphan_skips_when_other_tablet_import_present() {
        let src = "import Tablet.Bar\nimport Mathlib.Data.Real.Basic\n\ntheorem Foo : True := by trivial\n";
        assert!(super::ensure_preamble_import_for_orphan(src, "Foo").is_none());
    }

    #[test]
    fn ensure_preamble_import_for_orphan_skips_for_preamble_itself() {
        let src = "import Mathlib.Topology.Basic\n";
        assert!(super::ensure_preamble_import_for_orphan(src, "Preamble").is_none());
    }

    #[test]
    fn ensure_preamble_import_for_orphan_inserts_at_top_when_no_existing_imports() {
        let src = "-- [TABLET NODE: Foo]\ntheorem Foo : True := by trivial\n";
        let out = super::ensure_preamble_import_for_orphan(src, "Foo").expect("should rewrite");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "import Tablet.Preamble");
        assert_eq!(lines[1], "-- [TABLET NODE: Foo]");
    }

    #[test]
    fn normalize_node_lean_imports_on_disk_writes_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("Tablet")).unwrap();
        let path = dir.path().join("Tablet").join("Foo.lean");
        std::fs::write(
            &path,
            "import Mathlib.Topology.Basic\n\ntheorem Foo : True := by trivial\n",
        )
        .unwrap();
        let modified = super::normalize_node_lean_imports_on_disk(dir.path(), "Foo").unwrap();
        assert!(modified);
        let new = std::fs::read_to_string(&path).unwrap();
        assert!(new.contains("import Tablet.Preamble"));
        // Idempotent.
        let modified_again = super::normalize_node_lean_imports_on_disk(dir.path(), "Foo").unwrap();
        assert!(!modified_again);
    }

    #[test]
    fn normalize_node_lean_imports_on_disk_returns_false_when_file_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let modified = super::normalize_node_lean_imports_on_disk(dir.path(), "Missing").unwrap();
        assert!(!modified);
    }
}
