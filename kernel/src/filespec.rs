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
    // Strip leading `@[...]` attribute groups before tokenizing for the
    // declaration keyword. PV extraction-model constant defs carry inline
    // attributes — e.g. `@[global_simps, irreducible] def FIELD_MODULUS …` —
    // whose first whitespace token (`@[global_simps,`) is not a keyword, so a
    // raw first-token scan returns `None` and the principal-name parse fails at
    // worker acceptance. We strip only the attribute groups here (not the
    // `noncomputable` modifier) so the `noncomputable def`/`noncomputable
    // theorem` kind below is still recognized. This is parsing-only: it never
    // rewrites the file, so the byte-pinned slice is unaffected.
    let trimmed = strip_leading_attribute_groups(trimmed);
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
    if [
        "theorem",
        "lemma",
        "def",
        "abbrev",
        "example",
        "structure",
        "inductive",
        "class",
        "instance",
        // FIX 3 (defense-in-depth): recognize `axiom` and `opaque` as
        // declaration heads too. A node authoring a bare `axiom Owner.eq_1`
        // was previously invisible to this scanner (the `axiom` keyword was
        // absent from the list), so a forged in-file axiom slipped past the
        // shape check. Recognizing them also lets a legitimate project-axiom
        // / opaque principal declaration satisfy the principal-name check.
        "axiom",
        "opaque",
    ]
    .contains(&tokens[0])
        && tokens.len() >= 2
    {
        return Some((tokens[0].to_string(), tokens[1].to_string()));
    }
    None
}

/// True iff a Lean declaration `kind` (as produced by [`parse_decl_line`])
/// names a definition-family declaration: `def` / `abbrev` /
/// `noncomputable def`, plus the data-declaration kinds `structure`,
/// `inductive`, and `class`. These are non-proof-bearing — they never
/// enter soundness or proof-formalization as proof targets and pair with
/// the `.tex` `definition` environment.
pub fn is_definition_kind(kind: &str) -> bool {
    kind.contains("def")
        || kind == "abbrev"
        || kind == "structure"
        || kind == "inductive"
        || kind == "class"
}

/// True iff a Lean declaration `kind` is proof-bearing: theorem-family
/// declarations, plus `instance` (an instance discharges the class's law
/// fields, which can carry `sorry`s and close during proof-formalization,
/// so every instance routes through soundness and pairs with a
/// `lemma`-style `.tex` claim).
pub fn is_proof_bearing_kind(kind: &str) -> bool {
    kind == "instance"
        || kind == "theorem"
        || kind == "lemma"
        || kind == "noncomputable theorem"
        || kind == "example"
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

/// The reserved declaration-head keywords whose declId we check for a
/// reserved-shaped final component. A superset of the principal-shape
/// keywords: it adds `axiom`/`opaque` (FIX 3) since a forged in-file axiom
/// is exactly one of the surfaces we must catch.
const RESERVED_SCAN_DECL_KEYWORDS: &[&str] = &[
    "theorem", "lemma", "def", "abbrev", "structure", "inductive", "class", "instance", "axiom",
    "opaque",
];

/// Leading declaration modifiers that may precede the declaration keyword.
/// The Rust first-token line-scanner (`parse_decl_line`) keys off the FIRST
/// token, so a `protected`/`private` prefix defeats it — which is the exact
/// bypass FIX 2 closes. This backstop strips these before looking for the
/// keyword. Mirrors `Lean.Parser.Command.declModifiers` plus the
/// `attrKind`/visibility atoms that may lead an `instance`/`def`.
const LEADING_DECL_MODIFIERS: &[&str] = &[
    "private",
    "protected",
    "noncomputable",
    "unsafe",
    "partial",
    "nonrec",
    "meta",
    "scoped",
    "local",
];

/// MACRO BAN: command keywords that DEFINE a command macro, term/command
/// syntax, or elaborator — the mechanisms that can synthesize a top-level
/// declaration whose name never appears literally in source (e.g. a `macro`
/// emitting `theorem Owner.eq_1`). An ordinary Tablet node file may not
/// author any of these. The keyword set mirrors the Lean command syntax
/// kinds banned in `scripts/lean_local_closure.lean::bannedCommandKinds`
/// (verified against v4.30.0-rc1). `notation`/`infix`/`infixl`/`infixr`/
/// `prefix`/`postfix` are intentionally ABSENT: they introduce term-level
/// notation only (RHS is a term) and cannot declare a top-level constant.
///
/// This text scan is **defense-in-depth**; the authoritative gate is the
/// Lean-parse owner-file scan in `lean_local_closure.lean`, which runs for
/// every node kind via the `--scan-only` mode and is robust to attribute /
/// `… in` wrappers and arbitrary layout the text scan handles only
/// best-effort.
const MACRO_DEFINING_COMMAND_KEYWORDS: &[&str] = &[
    "macro",
    "macro_rules",
    "elab",
    "elab_rules",
    "syntax",
    "declare_syntax_cat",
    "binder_predicate",
];

/// Strip a leading `<command> in ` wrapper (`set_option x in …`,
/// `open Lean in …`, `attribute […] in …`) so the macro-command backstop
/// sees the wrapped command's head. Best-effort, single-level: a banned
/// command may be wrapped by one of these `… in` combinators, which the
/// authoritative Lean-parse scan handles via its recursive tree walk; the
/// text scan strips the longest leading `… in` run on the line. We only
/// strip when the line begins with a known wrapper keyword to avoid eating
/// an `in` that is part of a term.
fn strip_leading_in_wrappers(line: &str) -> &str {
    const IN_WRAPPER_HEADS: &[&str] = &["set_option", "open", "attribute", "variable", "universe"];
    let mut rest = line.trim_start();
    loop {
        let head = rest.split_whitespace().next().unwrap_or("");
        if !IN_WRAPPER_HEADS.contains(&head) {
            return rest;
        }
        // Find a whitespace-delimited `in` token and continue past it.
        match find_in_token(rest) {
            Some(after) => {
                rest = after.trim_start();
                continue;
            }
            None => return rest,
        }
    }
}

/// Return the slice after the first whitespace-delimited `in` token in
/// `line`, or `None` if there is no such token.
fn find_in_token(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        // Skip leading whitespace of the next token.
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        let start = idx;
        while idx < bytes.len() && !bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if &line[start..idx] == "in" {
            return Some(&line[idx..]);
        }
        if start == idx {
            break;
        }
    }
    None
}

/// True iff a single name component string is "reserved-shaped" — i.e. it
/// mirrors the `.str` cases of `Lean.Name.isInternalDetail`
/// (`Lean/Data/Name.lean`): a leading `_`, or `eq_`/`match_`/`proof_`/
/// `omega_` followed only by digits/`_`. This is the textual shape a
/// compiler-generated internal uses; an AUTHORED declaration with this final
/// component is an unregistered private auxiliary masquerading as generated.
///
/// We check the FINAL component only, never the full dotted name: the
/// ancestor / `.num` cases of `isInternalDetail` describe generated /
/// non-authorable shapes that do not apply to an authored final component.
pub fn is_reserved_shaped_component(component: &str) -> bool {
    fn match_prefix(s: &str, pre: &str) -> bool {
        // `s` begins with `pre`, then is only digits / '_'. Verbatim port of
        // `Lean.Name.isInternalDetail.matchPrefix`.
        s.strip_prefix(pre)
            .map(|rest| rest.chars().all(|c| c.is_ascii_digit() || c == '_'))
            .unwrap_or(false)
    }
    component.starts_with('_')
        || match_prefix(component, "eq_")
        || match_prefix(component, "match_")
        || match_prefix(component, "proof_")
        || match_prefix(component, "omega_")
}

/// Strip leading attribute groups (`@[...]`) and declaration modifiers from a
/// trimmed line, returning the remainder beginning at the declaration
/// keyword (or whatever follows the modifiers). Best-effort: a multi-line
/// `@[...]` attribute is not joined here (the backstop is defense-in-depth;
/// the authoritative Lean-parse scan in `scripts/lean_local_closure.lean`
/// handles arbitrary layout).
fn strip_leading_decl_modifiers(line: &str) -> &str {
    let mut rest = line.trim_start();
    loop {
        let after_attrs = strip_leading_attribute_groups(rest);
        if after_attrs.len() != rest.len() {
            // An attribute group was stripped; loop to handle a following
            // modifier or another attribute group.
            rest = after_attrs;
            continue;
        }
        let first = rest.split_whitespace().next().unwrap_or("");
        if LEADING_DECL_MODIFIERS.contains(&first) {
            rest = rest[first.len()..].trim_start();
            continue;
        }
        return rest;
    }
}

/// Strip leading `@[...]` attribute groups from a trimmed line, returning the
/// remainder beginning at the first non-attribute token. Best-effort: a
/// multi-line `@[...]` attribute (matching `]` not on this line) is not joined
/// — we stop at the unbalanced group. Declaration modifiers are NOT stripped
/// (so a caller that distinguishes `noncomputable def` still sees the
/// `noncomputable` token).
pub fn strip_leading_attribute_groups(line: &str) -> &str {
    let mut rest = line.trim_start();
    while let Some(after) = rest.strip_prefix("@[") {
        match after.find(']') {
            Some(idx) => {
                rest = after[idx + 1..].trim_start();
            }
            None => return rest, // unbalanced on this line; give up here.
        }
    }
    rest
}

/// FIX 2 universal backstop. Scan `lean_content` for AUTHORED declarations
/// whose final name component is reserved-shaped, returning the offending
/// final components (deduplicated). Modifier-aware: it strips leading
/// `protected`/`private`/attribute prefixes that defeat the first-token
/// `parse_decl_line` scanner, then reads the declId token and takes its final
/// dotted component (so both `Owner.eq_1` and a `namespace Owner … theorem
/// eq_1` form surface `eq_1`).
///
/// This is **defense-in-depth**, NOT the primary gate (a text scan cannot see
/// `where`/`let rec`-bound auxiliaries or names produced by macros, and does
/// not resolve layout perfectly). The authoritative, all-surfaces gate is the
/// Lean-parse owner-file scan in `scripts/lean_local_closure.lean`, which runs
/// on every node the local-closure probe runs on. This backstop gives
/// earlier, clearer, UNIVERSAL (every node kind) worker feedback for the
/// common top-level surfaces. The principal declaration (final component ==
/// `node_name`) is never flagged.
pub fn reserved_shaped_authored_components(lean_content: &str, node_name: &str) -> Vec<String> {
    let mut offenders: Vec<String> = Vec::new();
    for raw in lean_content.lines() {
        let line = strip_leading_decl_modifiers(raw.trim());
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 2 {
            continue;
        }
        if !RESERVED_SCAN_DECL_KEYWORDS.contains(&tokens[0]) {
            continue;
        }
        // The declId token. Strip any `.{...}` universe-binder suffix and a
        // trailing `:`/`(`/`{`/`[` that may abut a no-space signature.
        let decl_id = tokens[1];
        let decl_id = decl_id.split(".{").next().unwrap_or(decl_id);
        let decl_id = decl_id
            .trim_end_matches(|c: char| c == ':' || c == '(' || c == '{' || c == '[');
        // Final dotted component.
        let Some(final_component) = decl_id.rsplit('.').next() else {
            continue;
        };
        if final_component.is_empty()
            || final_component == node_name
            || decl_name_matches_node(decl_id, node_name)
        {
            continue;
        }
        if is_reserved_shaped_component(final_component) && !offenders.iter().any(|o| o == final_component)
        {
            offenders.push(final_component.to_string());
        }
    }
    offenders
}

/// MACRO BAN backstop. Scan `lean_content` for macro/syntax/elaborator-
/// DEFINING commands and return the offending keywords (deduplicated, in
/// first-seen order). Such a command can synthesize a top-level declaration
/// whose name never appears literally in source, bypassing the
/// local-closure/authored-name invariant. Modifier-aware (strips leading
/// `scoped`/`local`/attribute prefixes) and `… in`-wrapper-aware (strips a
/// leading `set_option … in` / `open … in` so the wrapped command head is
/// seen).
///
/// This is **defense-in-depth**, NOT the primary gate: a line-oriented text
/// scan cannot resolve every layout (a banned command head split across
/// lines, or buried in a deeper `… in` nesting) and does not parse syntax.
/// The authoritative, all-surfaces gate is the Lean-parse owner-file scan in
/// `scripts/lean_local_closure.lean`, which runs for every node kind via the
/// `--scan-only` mode. This backstop gives earlier, clearer, universal
/// worker feedback for the common surface (a `macro`/`elab`/`syntax`/… line).
pub fn macro_defining_commands(lean_content: &str) -> Vec<String> {
    let mut offenders: Vec<String> = Vec::new();
    for raw in lean_content.lines() {
        // Strip a leading `… in` wrapper, then leading modifiers/attributes,
        // then read the head keyword.
        let line = strip_leading_in_wrappers(raw.trim());
        let line = strip_leading_decl_modifiers(line);
        let head = line.split_whitespace().next().unwrap_or("");
        if MACRO_DEFINING_COMMAND_KEYWORDS.contains(&head) && !offenders.iter().any(|o| o == head) {
            offenders.push(head.to_string());
        }
    }
    offenders
}

/// A principal declaration may be namespaced/dotted (`FiniteDecimal.value`
/// in node file `FiniteDecimal_value.lean`); it matches its node when the
/// dot-sanitized full name equals the node stem — the same sanitized-stem
/// rule the fingerprint observer applies (`lean_node_is_definition_like`).
/// dec2flt request 952 (2026-07-04): full worker acceptance rejected an
/// untouched baseline node over exactly this, halting the run.
pub fn decl_name_matches_node(decl_name: &str, node_name: &str) -> bool {
    decl_name == node_name || decl_name.replace('.', "_") == node_name
}

pub fn validate_lean_node_shape(lean_content: &str, node_name: &str) -> Vec<String> {
    let heads = declaration_heads(lean_content);
    let matching: Vec<&DeclarationHead> = heads
        .iter()
        .filter(|head| decl_name_matches_node(&head.name, node_name))
        .collect();
    let mut errors = Vec::new();

    // MACRO BAN universal backstop: reject a node that defines a
    // macro/syntax/elaborator command. Such a command can synthesize a
    // top-level declaration whose name never appears literally in source
    // (e.g. a `macro` emitting `theorem Owner.eq_1`), bypassing the
    // local-closure/authored-name invariant. This is defense-in-depth; the
    // authoritative gate is the Lean-parse owner-file scan in
    // `lean_local_closure.lean`. Checked before the reserved-name backstop
    // because a macro command is the more fundamental violation (it can
    // emit the reserved-shaped name the text scan below cannot see).
    for keyword in macro_defining_commands(lean_content) {
        errors.push(format!(
            "Lean node file defines a `{keyword}` command: an ordinary Tablet node file may not author a macro/syntax/elaborator-defining command (`macro`/`macro_rules`/`elab`/`elab_rules`/`syntax`/`declare_syntax_cat`/`binder_predicate`). Such a command can synthesize a top-level declaration whose name never appears literally in source, bypassing the local-closure/authored-name invariant. Remove the command (term-level `notation`/`infix`/`prefix`/`postfix` are allowed)."
        ));
    }

    // FIX 2 universal backstop: reject a node that authors a declaration
    // whose final name component is reserved-shaped (an `isInternalDetail`
    // shape — leading `_`, or `eq_`/`match_`/`proof_`/`omega_` + digits).
    // Such a declaration is an unregistered private auxiliary masquerading as
    // a compiler-generated artifact; the local-closure probe's transparent
    // walk would hide it, letting it cross a node boundary untracked. This
    // text-scan is defense-in-depth (it runs universally, for every node
    // kind, and gives early worker feedback); the authoritative all-surfaces
    // gate is the Lean-parse owner-file scan in `lean_local_closure.lean`.
    for offender in reserved_shaped_authored_components(lean_content, node_name) {
        errors.push(format!(
            "Lean node file authors a reserved-shaped private auxiliary declaration `{offender}`: a declaration whose final name component matches a compiler-internal shape (leading `_`, or `eq_`/`match_`/`proof_`/`omega_` followed only by digits/`_`) is an unregistered cross-node dependency hidden from the local-closure check. Move the auxiliary into its own registered node, or rename it to a non-reserved shape."
        ));
    }
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
        .filter(|head| !decl_name_matches_node(&head.name, node_name))
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

/// Declaration-kind-specific FILESPEC rules for the principal declaration
/// named `node_name`. Two mandates:
///
///   * **Named instance.** The kernel identifies a node's principal
///     declaration by name, so an anonymous instance
///     (`instance : ClassName … := …`) has no name to match the node and
///     is rejected.
///   * **`inductive` `where`-form.** An `inductive` node must declare its
///     constructors below the `-- BODY` marker via the `where` form
///     (`inductive Foo : Type where`), so the body delimiter is uniformly
///     `where`. The bare-pipe form (`inductive Foo | c1 | c2`) is
///     rejected.
///
/// Returns one error string per violation (empty when the principal
/// declaration is absent or of a kind these rules do not constrain).
pub fn validate_declaration_kind_shape(lean_content: &str, node_name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let heads = declaration_heads(lean_content);
    let Some(head) = heads
        .iter()
        .find(|h| decl_name_matches_node(&h.name, node_name))
    else {
        // An anonymous `instance` parses with `name == ":"`, so the
        // principal-name lookup above misses it. Detect that shape
        // directly and surface the named-instance mandate.
        if heads
            .iter()
            .any(|h| h.kind == "instance" && !is_valid_decl_name(&h.name))
        {
            errors.push(
                "Anonymous instance is not allowed; give the instance a name (e.g. `instance NodeName : ClassName … := …`) so the kernel can identify the node's principal declaration.".to_string(),
            );
        }
        return errors;
    };

    if head.kind == "instance" && !is_valid_decl_name(&head.name) {
        errors.push(
            "Anonymous instance is not allowed; give the instance a name (e.g. `instance NodeName : ClassName … := …`) so the kernel can identify the node's principal declaration.".to_string(),
        );
    }

    if head.kind == "inductive" && !inductive_uses_where_form(lean_content, node_name) {
        errors.push(
            "An inductive node must declare its constructors with the `where` form (`inductive Foo : Type where`) so the constructors sit below `-- BODY`; the bare-pipe form (`inductive Foo | c1 | c2`) is not allowed.".to_string(),
        );
    }

    errors
}

fn is_valid_decl_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if ch.is_ascii_alphabetic() || ch == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.')
}

/// Scan the principal `inductive` declaration's header for the `where`
/// keyword. The header runs from the declaration line through the line
/// holding the `-- BODY` marker's predecessor; an `inductive` in the
/// `where` form has `where` as the last token of the line above the
/// marker (FILESPEC body-delimiter rule). We accept `where` appearing on
/// any header line up to and including that body-delimiter line.
fn inductive_uses_where_form(lean_content: &str, node_name: &str) -> bool {
    let mut in_decl = false;
    for line in lean_content.lines() {
        let trimmed = line.trim();
        if let Some((kind, name)) = parse_decl_line(trimmed) {
            if decl_name_matches_node(&name, node_name) && kind == "inductive" {
                in_decl = true;
            } else if in_decl {
                // A new declaration begins: the inductive header ended.
                break;
            }
        }
        if trimmed == "-- BODY" {
            break;
        }
        if in_decl && header_line_has_where_keyword(trimmed) {
            return true;
        }
    }
    false
}

/// True iff `where` appears as a standalone keyword token in `line`
/// (whitespace-delimited), not as a substring of an identifier.
fn header_line_has_where_keyword(line: &str) -> bool {
    line.split_whitespace().any(|tok| {
        tok == "where"
            || tok
                .strip_suffix("where")
                .is_some_and(|p| !p.ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_'))
    })
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
        declaration_heads, is_definition_kind, is_proof_bearing_kind, is_reserved_shaped_component,
        macro_defining_commands, parse_top_level_envs, reserved_shaped_authored_components,
        tex_statement_environment, validate_declaration_kind_shape, validate_lean_node_shape,
        validate_tex_format,
    };

    #[test]
    fn recognizes_all_four_new_declaration_kinds() {
        let heads = declaration_heads(
            "structure S where\n  x : Nat\ninductive I : Type where\n  | c\nclass C where\n  f : Nat\ninstance Inst : C where\n  f := 0\n",
        );
        let kinds: Vec<(&str, &str)> = heads
            .iter()
            .map(|h| (h.kind.as_str(), h.name.as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("structure", "S"),
                ("inductive", "I"),
                ("class", "C"),
                ("instance", "Inst"),
            ]
        );
    }

    #[test]
    fn structure_inductive_class_are_definition_kinds_instance_is_proof_bearing() {
        for kind in ["structure", "inductive", "class", "def", "abbrev", "noncomputable def"] {
            assert!(is_definition_kind(kind), "{kind} should be definition");
            assert!(!is_proof_bearing_kind(kind), "{kind} should not be proof-bearing");
        }
        assert!(is_proof_bearing_kind("instance"));
        assert!(!is_definition_kind("instance"));
        assert!(is_proof_bearing_kind("theorem"));
        assert!(is_proof_bearing_kind("lemma"));
    }

    #[test]
    fn anonymous_instance_is_rejected() {
        let content = "-- [TABLET NODE: Foo]\ninstance : Group T := by\n-- BODY\n  sorry\n";
        let errors = validate_declaration_kind_shape(content, "Foo");
        assert!(
            errors.iter().any(|e| e.contains("Anonymous instance")),
            "expected anonymous-instance rejection, got {errors:?}"
        );
    }

    #[test]
    fn named_instance_is_accepted() {
        let content =
            "-- [TABLET NODE: Foo]\ninstance Foo : Group T := by\n-- BODY\n  sorry\n";
        assert!(validate_declaration_kind_shape(content, "Foo").is_empty());
    }

    #[test]
    fn inductive_where_form_is_accepted_bare_pipe_is_rejected() {
        let where_form =
            "-- [TABLET NODE: Foo]\ninductive Foo : Type where\n-- BODY\n  | c1\n  | c2\n";
        assert!(validate_declaration_kind_shape(where_form, "Foo").is_empty());

        let bare_pipe = "-- [TABLET NODE: Foo]\ninductive Foo\n-- BODY\n  | c1\n  | c2\n";
        let errors = validate_declaration_kind_shape(bare_pipe, "Foo");
        assert!(
            errors.iter().any(|e| e.contains("where")),
            "expected inductive where-form mandate, got {errors:?}"
        );
    }

    #[test]
    fn structure_and_class_have_no_kind_shape_constraints() {
        let s = "-- [TABLET NODE: Foo]\nstructure Foo where\n-- BODY\n  x : Nat\n";
        assert!(validate_declaration_kind_shape(s, "Foo").is_empty());
        let c = "-- [TABLET NODE: Foo]\nclass Foo where\n-- BODY\n  f : Nat\n";
        assert!(validate_declaration_kind_shape(c, "Foo").is_empty());
    }

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
    fn dotted_principal_declaration_matches_its_sanitized_node_stem() {
        // dec2flt request 952 (2026-07-04): `Tablet/FiniteDecimal_value.lean`
        // declares `def FiniteDecimal.value`; full worker acceptance rejected
        // the untouched baseline node ("must contain a top-level declaration
        // named FiniteDecimal_value" + the dotted decl listed as an
        // additional declaration) and halted the run. The sanitized-stem
        // rule the fingerprint observer already applies governs here too.
        let content = "-- [TABLET NODE: FiniteDecimal_value]\nimport Tablet.FiniteDecimal\n\n\
                       noncomputable def FiniteDecimal.value (d : FiniteDecimal) : ℝ :=\n-- BODY\n  0\n";
        let errors = validate_lean_node_shape(content, "FiniteDecimal_value");
        assert!(errors.is_empty(), "{errors:?}");
        // The kind rules find the dotted principal too (no anonymous-instance
        // misfire, no missing-principal early return).
        assert!(validate_declaration_kind_shape(content, "FiniteDecimal_value").is_empty());
        // A genuinely unrelated declaration still trips the shape check.
        let with_stray = format!("{content}\ntheorem Unrelated : True := by\n  trivial\n");
        let errors = validate_lean_node_shape(&with_stray, "FiniteDecimal_value");
        assert!(
            errors.iter().any(|e| e.contains("additional declarations")),
            "{errors:?}"
        );
        // And a dotted name that does NOT sanitize to the stem is not a match.
        let errors = validate_lean_node_shape(content, "SomethingElse");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("must contain a top-level declaration named SomethingElse")),
            "{errors:?}"
        );
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

    // ---- FIX 2 backstop + FIX 3 unit tests ----

    #[test]
    fn reserved_shaped_component_matches_lean_isinternaldetail_string_cases() {
        // Reserved (mirrors Name.isInternalDetail's `.str` cases).
        for s in ["_helper", "_hidden", "_sunfold", "eq_1", "match_12", "proof_3", "omega_0", "eq_", "eq__"] {
            assert!(is_reserved_shaped_component(s), "{s} should be reserved-shaped");
        }
        // Not reserved.
        for s in ["helper", "realAux", "eq", "equiv", "eqOf", "matcher", "proof", "omega", "eq_def", "eq_1a", "Foo"] {
            assert!(!is_reserved_shaped_component(s), "{s} should NOT be reserved-shaped");
        }
        // `eq_1a` has a non-digit/non-`_` char after `eq_`, so matchPrefix is false.
        assert!(!is_reserved_shaped_component("eq_1a"));
        // `eq_def` is reserved by the env registry, not by the name shape, so
        // the SHAPE test (this function) correctly returns false; the probe's
        // `isReservedName` half covers it.
        assert!(!is_reserved_shaped_component("eq_def"));
    }

    #[test]
    fn backstop_catches_bare_and_modifier_prefixed_reserved_auxes() {
        // Bare dotted.
        assert_eq!(
            reserved_shaped_authored_components(
                "theorem Owner : True := trivial\ntheorem Owner.eq_1 : True := trivial\n",
                "Owner"
            ),
            vec!["eq_1".to_string()]
        );
        // `protected` prefix (the line-scanner bypass) is stripped + caught.
        assert_eq!(
            reserved_shaped_authored_components(
                "theorem Owner : True := trivial\nprotected theorem Owner.eq_1 : True := by decide\n",
                "Owner"
            ),
            vec!["eq_1".to_string()]
        );
        // `private` prefix.
        assert_eq!(
            reserved_shaped_authored_components(
                "private theorem Owner._helper : True := trivial\n",
                "Owner"
            ),
            vec!["_helper".to_string()]
        );
        // `namespace`-relative form: written declId is the bare `eq_1`.
        assert_eq!(
            reserved_shaped_authored_components(
                "namespace Owner\ntheorem eq_1 : True := trivial\nend Owner\n",
                "Owner"
            ),
            vec!["eq_1".to_string()]
        );
        // Forged `axiom` (FIX 3 makes it a recognized head here too).
        assert_eq!(
            reserved_shaped_authored_components("axiom Owner.eq_1 : True\n", "Owner"),
            vec!["eq_1".to_string()]
        );
        // `@[simp]`-attributed.
        assert_eq!(
            reserved_shaped_authored_components(
                "@[simp] protected theorem Owner._x : True := trivial\n",
                "Owner"
            ),
            vec!["_x".to_string()]
        );
    }

    #[test]
    fn backstop_no_false_positives() {
        // Principal (final component == node) is never flagged.
        assert!(reserved_shaped_authored_components("theorem Owner : True := trivial\n", "Owner")
            .is_empty());
        // Non-reserved auxiliary.
        assert!(reserved_shaped_authored_components(
            "protected theorem Owner.helper : True := trivial\n",
            "Owner"
        )
        .is_empty());
        // A genuine recursive def's SOURCE has no reserved-shaped declaration
        // commands (the `_sunfold`/`match_1`/`eq_1` are generated, not
        // authored), so a text scan of the source flags nothing.
        assert!(reserved_shaped_authored_components(
            "def RecDef : Nat -> Nat\n  | 0 => 0\n  | (n+1) => (RecDef n) + 1\n",
            "RecDef"
        )
        .is_empty());
        // A plain `let eq_1` inside a proof body is not a declaration head.
        assert!(reserved_shaped_authored_components(
            "theorem Owner : True := by\n  let eq_1 := 5\n  trivial\n",
            "Owner"
        )
        .is_empty());
    }

    #[test]
    fn validate_lean_node_shape_rejects_protected_reserved_aux() {
        // End-to-end through the universal chokepoint: a `protected` reserved
        // aux (which `parse_decl_line` alone misses) is rejected.
        let errors = validate_lean_node_shape(
            "-- [TABLET NODE: Owner]\nimport Tablet.Preamble\n\ntheorem Owner : True := trivial\nprotected theorem Owner.eq_1 : True := by decide\n",
            "Owner",
        );
        assert!(
            errors.iter().any(|e| e.contains("reserved-shaped") && e.contains("eq_1")),
            "expected reserved-shaped rejection naming eq_1, got {errors:?}"
        );
    }

    #[test]
    fn validate_lean_node_shape_allows_legit_protected_aux() {
        // A legitimate non-reserved protected auxiliary must not be flagged by
        // the reserved-shape backstop. (The principal-shape check still
        // permits one principal + auxiliaries under the node namespace.)
        let errors = validate_lean_node_shape(
            "-- [TABLET NODE: Owner]\nimport Tablet.Preamble\n\ntheorem Owner : True := trivial\nprotected theorem Owner.helper : True := by decide\n",
            "Owner",
        );
        assert!(
            !errors.iter().any(|e| e.contains("reserved-shaped")),
            "legit protected aux must not trigger reserved-shape rejection, got {errors:?}"
        );
    }

    #[test]
    fn parse_decl_line_strips_inline_attributes() {
        use super::{declaration_heads, parse_decl_line};
        // PV extraction-model single-line constant def with inline attributes.
        assert_eq!(
            parse_decl_line(
                "@[global_simps, irreducible] def FIELD_MODULUS : Std.I32 := 3329#i32"
            ),
            Some(("def".to_string(), "FIELD_MODULUS".to_string()))
        );
        // Single attribute.
        assert_eq!(
            parse_decl_line("@[reducible] def f := 0"),
            Some(("def".to_string(), "f".to_string()))
        );
        // Attribute + `noncomputable` combo: the keyword is still found AND the
        // compound kind is preserved (this is the only modifier `parse_decl_line`
        // looks through; `protected`/`private` are deliberately NOT stripped here
        // to keep all-math `declaration_heads` byte-identical — math principal
        // decls don't carry inline attributes, and the original scanner never
        // handled a bare `protected`/`private` lead either).
        assert_eq!(
            parse_decl_line("@[simp] protected theorem Foo : True := trivial"),
            None
        );
        // `noncomputable def` kind is preserved when an attribute leads it.
        assert_eq!(
            parse_decl_line("@[inline] noncomputable def g : Nat := 0"),
            Some(("noncomputable def".to_string(), "g".to_string()))
        );
        // Bare `noncomputable def` (no attribute) still keeps its compound kind.
        assert_eq!(
            parse_decl_line("noncomputable def h : Nat := 0"),
            Some(("noncomputable def".to_string(), "h".to_string()))
        );
        // Multi-attribute group with a theorem head.
        assert_eq!(
            parse_decl_line("@[a, b, c] theorem T : True := trivial"),
            Some(("theorem".to_string(), "T".to_string()))
        );
        // declaration_heads surfaces the attribute-prefixed principal head.
        let heads = declaration_heads(
            "@[global_simps, irreducible] def FIELD_MODULUS : Std.I32 := 3329#i32\n",
        );
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].kind, "def");
        assert_eq!(heads[0].name, "FIELD_MODULUS");
    }

    #[test]
    fn pv_shaped_node_passes_lean_node_shape() {
        // A full PV-shaped ExtractionModel constant-def node (single-line
        // attribute-prefixed principal def) passes the shape check: the
        // principal declaration is found, and nothing extra/forbidden is
        // flagged.
        let content = "-- [TABLET NODE: FIELD_MODULUS]\nimport Tablet.Preamble\n\n@[global_simps, irreducible] def FIELD_MODULUS : Std.I32 := 3329#i32\n";
        let errors = validate_lean_node_shape(content, "FIELD_MODULUS");
        assert!(
            errors.is_empty(),
            "PV-shaped attribute-prefixed def node should pass shape check, got {errors:?}"
        );
    }

    #[test]
    fn declaration_heads_recognizes_axiom_and_opaque() {
        // FIX 3: `axiom`/`opaque` are now recognized declaration heads.
        let heads = declaration_heads("axiom Foo : True\nopaque Bar : Nat := 0\n");
        let kinds: Vec<(&str, &str)> = heads
            .iter()
            .map(|h| (h.kind.as_str(), h.name.as_str()))
            .collect();
        assert_eq!(kinds, vec![("axiom", "Foo"), ("opaque", "Bar")]);
    }

    // ---- MACRO BAN backstop unit tests ----

    #[test]
    fn macro_ban_backstop_catches_each_command_family() {
        // Each banned command keyword is flagged, including modifier- and
        // `… in`-wrapped forms the bare first-token scan would miss.
        for (src, kw) in [
            ("macro \"op\" : term => `(1)\n", "macro"),
            ("macro_rules | `(1) => `(2)\n", "macro_rules"),
            ("elab \"op\" : term => return default\n", "elab"),
            ("elab_rules : term | `(1) => `(2)\n", "elab_rules"),
            ("syntax \"op\" : term\n", "syntax"),
            ("declare_syntax_cat mycat\n", "declare_syntax_cat"),
            ("binder_predicate x \" > \" y => `($x > $y)\n", "binder_predicate"),
            // Modifier-prefixed (a `scoped macro` defeats a first-token scan).
            ("scoped macro \"op\" : term => `(1)\n", "macro"),
            // `… in`-wrapped (top syntax kind is `Command.in`).
            ("set_option foo true in macro \"op\" : term => `(1)\n", "macro"),
            ("open Lean in elab \"op\" : term => return default\n", "elab"),
            // Attribute-prefixed.
            ("@[inherit_doc] macro \"op\" : term => `(1)\n", "macro"),
        ] {
            assert_eq!(
                macro_defining_commands(src),
                vec![kw.to_string()],
                "expected `{kw}` flagged for source {src:?}"
            );
        }
    }

    #[test]
    fn macro_ban_backstop_allows_notation_and_mixfix() {
        // Term-level notation / mixfix are ALLOWED: they cannot declare a
        // top-level constant. The scan must not flag them.
        for src in [
            "notation:max \"[[\" x \"]]\" => x\n",
            "infix:65 \" +++ \" => Nat.add\n",
            "infixl:65 \" +++ \" => Nat.add\n",
            "infixr:65 \" +++ \" => Nat.add\n",
            "prefix:max \"!!\" => Not\n",
            "postfix:max \"!\" => Nat.succ\n",
        ] {
            assert!(
                macro_defining_commands(src).is_empty(),
                "term-level notation/mixfix must be allowed: {src:?}"
            );
        }
    }

    #[test]
    fn macro_ban_backstop_no_false_positive_on_ordinary_nodes() {
        // Ordinary declarations — including ones whose identifiers or string
        // literals merely CONTAIN a banned keyword substring — are not flagged.
        for src in [
            "theorem Foo : True := trivial\n",
            "def macroHelper : Nat := 0\n", // identifier starts with "macro" but is not the keyword
            "def elaborate : Nat := 0\n",   // "elab" is a substring, not the head token
            "theorem T : True := by\n  let x := 1 -- syntax in a comment\n  trivial\n",
            "structure S where\n  x : Nat\n",
        ] {
            assert!(
                macro_defining_commands(src).is_empty(),
                "ordinary node must not be flagged: {src:?}"
            );
        }
    }

    #[test]
    fn validate_lean_node_shape_rejects_macro_command() {
        // End-to-end through the universal chokepoint: a node defining a
        // `macro` command is rejected with the macro-ban diagnostic.
        let errors = validate_lean_node_shape(
            "-- [TABLET NODE: Owner]\nimport Tablet.Preamble\n\ntheorem Owner : True := trivial\nmacro \"declare_eq1\" : command => `(theorem Owner.eq_1 : True := trivial)\n",
            "Owner",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("`macro` command") && e.contains("may not author")),
            "expected macro-ban rejection, got {errors:?}"
        );
    }

    #[test]
    fn validate_lean_node_shape_allows_notation_node() {
        // A node using only term-level `notation`/`infix` is not flagged by
        // the macro-ban backstop.
        let errors = validate_lean_node_shape(
            "-- [TABLET NODE: Owner]\nimport Tablet.Preamble\n\nnotation:max \"[[\" x \"]]\" => x\ntheorem Owner : ([[True]]) := trivial\n",
            "Owner",
        );
        assert!(
            !errors.iter().any(|e| e.contains("command")),
            "term-level notation node must not trigger macro-ban rejection, got {errors:?}"
        );
    }
}
