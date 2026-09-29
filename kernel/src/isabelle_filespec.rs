//! Isabelle/HOL `.thy` outer-syntax splitter — the Tier-1 file-format /
//! signature-identity layer, the Isabelle analogue of [`crate::filespec_split`]
//! + [`crate::filespec`] for Lean.
//!
//! ## Why this is a tokenizer, not a marker scan
//!
//! Lean's `filespec_split` keys off a single `-- BODY` line comment because a
//! Lean theorem is ONE command and the statement/proof boundary is an
//! in-command token. Isabelle is different (research
//! `03-file-format-hashing.md`, Part B): the **statement and proof are
//! separate outer-syntax commands**, so the natural boundary is a *command
//! boundary*, and there is no parser-inert line comment that reliably sits at
//! the right spot. A naive "split at the first `proof`/`by`/`using`" scan
//! mis-fires on the cartouche/string/comment hazards — exactly the bug class
//! FILESPEC v2 was created to escape. So Tier 1 here is a faithful port of
//! Isabelle's **outer lexer** (Part C, Option 2): a single left-to-right
//! tokenizer that correctly skips nested `(* … *)` comments, `"…"`/`` `…` ``
//! strings, `‹…›` cartouches (incl. the ASCII `\<open>…\<close>` spelling),
//! `{* … *}` verbatim, and `\<name>` symbol tokens, then splits the token
//! stream into **command spans** by a static keyword-KIND table.
//!
//! This is the prover-free hot path. The authoritative server cross-check
//! (Option 3 — confirm the real `Outer_Syntax` parsed the expected command-span
//! shape) lands at worker acceptance only, in the checker increment; it does
//! not sit on this per-node drift path.
//!
//! ## What this module is NOT
//!
//! It does **no inner-syntax (term) parsing**. Cartouche/string payloads are
//! opaque bytes. Symbol-SPELLING canonicalization (`∀`≡`\<forall>`,
//! `‹`≡`\<open>`) is byte-safe everywhere (it is how Isabelle's symbol layer
//! works), but no deeper semantic normalization is performed — Tier 2 (the
//! elaborated-meaning fingerprint) owns that and runs through the server.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::filespec_split::FilespecSplit;
use crate::model::NodeId;

// ===========================================================================
// Outer tokenizer
// ===========================================================================

/// One outer-syntax token's classification. Payload-bearing kinds
/// (`Comment`/`String`/`AltString`/`Cartouche`/`Verbatim`) carry their entire
/// delimited region (including delimiters) in the token span; their interior
/// is OPAQUE — no interior keyword can fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// An alphanumeric word: `theorem`, `Foo`, `using`, `qed`. The unit the
    /// keyword-kind table classifies. (Isabelle's `short_ident`/`long_ident`:
    /// `ident("."ident)*` — an INTERIOR dot keeps `Thm.add_oracle` one token,
    /// while a TRAILING `.`/`..` — a glued proof terminal — is split off as
    /// its own `Sym` token.)
    Ident,
    /// A symbolic-identifier run, e.g. `:`, `==>`, `⟹`. Also the bucket for a
    /// bare `\<name>` symbol token that is NOT a cartouche delimiter
    /// (`\<forall>`, `\<Rightarrow>`). Never a command keyword.
    Sym,
    /// `"…"` inner-syntax string (backslash escapes). Opaque payload.
    String,
    /// `` `…` `` alt-string. Opaque payload.
    AltString,
    /// `‹…›` / `\<open>…\<close>` cartouche (nested). Opaque payload.
    Cartouche,
    /// `{* … *}` legacy verbatim. Opaque payload.
    Verbatim,
    /// `(* … *)` source comment (nested). Opaque payload.
    Comment,
}

/// One outer token with its byte span `[start, end)` over the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

/// The ASCII spelling of the cartouche open delimiter (`\<open>`).
const OPEN_ASCII: &str = "\\<open>";
/// The ASCII spelling of the cartouche close delimiter (`\<close>`).
const CLOSE_ASCII: &str = "\\<close>";
/// The Unicode cartouche open delimiter `‹` (U+2039).
const OPEN_UNICODE: char = '\u{2039}';
/// The Unicode cartouche close delimiter `›` (U+203A).
const CLOSE_UNICODE: char = '\u{203A}';

/// True iff `c` may appear in a SINGLE component of an outer alphanumeric
/// identifier word: letters, digits, `_`, `'`. NOT `.` — Isabelle's long
/// identifiers are `ident("."ident)*` (a dot is part of the word only when
/// ANOTHER ident char follows it), which the ident arm of [`tokenize`]
/// implements with one char of lookahead. That keeps a qualified name like
/// `Thm.add_oracle` one token while a TRAILING `.`/`..` — the glued proof
/// terminal idiom `frac_neg_frac..` (see `HOL/Archimedean_Field.thy`) — is
/// split off as its own `Sym` token, so the keyword table and the goal-stack
/// tracker see the terminal exactly.
fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '\''
}

/// Tokenize Isabelle outer syntax. Left-to-right, single pass. Payload regions
/// (comment/string/altstring/cartouche/verbatim) are emitted as one opaque
/// token spanning the whole delimited region; everything else is split into
/// `Ident` words and `Sym` runs. Whitespace is not emitted.
///
/// Robust to the documented hazards (research Part B.1): nested `(* (* *) *)`,
/// nested cartouches `‹ ‹› ›`, the ASCII `\<open>…\<close>` cartouche spelling,
/// `"…"` with `\"` escapes, `` `…` `` alt-strings, `{* … *}` verbatim, and
/// `\<name>` symbol tokens (so a `\<close>` inside a `\<open>` nests, and a
/// `\<forall>` is a single `Sym` token that can never be read as a keyword).
pub fn tokenize(src: &str) -> Vec<Token> {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut tokens = Vec::new();
    let mut i = 0usize;

    while i < len {
        let rest = &src[i..];
        let b = bytes[i];

        // Whitespace: skip.
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // `(* … *)` nested source comment.
        if rest.starts_with("(*") {
            let end = scan_nested_comment(src, i);
            tokens.push(Token { kind: TokenKind::Comment, start: i, end });
            i = end;
            continue;
        }

        // `{* … *}` legacy verbatim (non-nesting).
        if rest.starts_with("{*") {
            let end = scan_until(src, i + 2, "*}");
            tokens.push(Token { kind: TokenKind::Verbatim, start: i, end });
            i = end;
            continue;
        }

        // `"…"` inner-syntax string with backslash escapes.
        if b == b'"' {
            let end = scan_quoted(src, i, '"');
            tokens.push(Token { kind: TokenKind::String, start: i, end });
            i = end;
            continue;
        }

        // `` `…` `` alt-string with backslash escapes.
        if b == b'`' {
            let end = scan_quoted(src, i, '`');
            tokens.push(Token { kind: TokenKind::AltString, start: i, end });
            i = end;
            continue;
        }

        // Cartouche — Unicode `‹…›`.
        if rest.starts_with(OPEN_UNICODE) {
            let end = scan_cartouche(src, i);
            tokens.push(Token { kind: TokenKind::Cartouche, start: i, end });
            i = end;
            continue;
        }

        // Cartouche — ASCII `\<open>…\<close>`. Only treat `\<open>` as a
        // cartouche opener; any other `\<name>` is a plain symbol token.
        if rest.starts_with(OPEN_ASCII) {
            let end = scan_cartouche(src, i);
            tokens.push(Token { kind: TokenKind::Cartouche, start: i, end });
            i = end;
            continue;
        }

        // `\<name>` symbol token (e.g. `\<forall>`, `\<Rightarrow>`, a stray
        // `\<close>`). One `Sym` token; never a keyword. Must come after the
        // `\<open>` cartouche check above.
        if rest.starts_with("\\<") {
            if let Some(rel_end) = rest.find('>') {
                let end = i + rel_end + 1;
                tokens.push(Token { kind: TokenKind::Sym, start: i, end });
                i = end;
                continue;
            }
            // Unterminated `\<…` — consume the backslash as a lone Sym.
            tokens.push(Token { kind: TokenKind::Sym, start: i, end: i + 1 });
            i += 1;
            continue;
        }

        // Identifier word. Gate on `is_ident_char` — the SAME predicate the
        // consume-loop below advances on. A Unicode letter/numeral (λ, α, CJK,
        // Arabic-Indic digit) is not an ident char here, so it falls through to
        // the guarded `Sym` arm (λ → `\<lambda>`) instead of producing a
        // zero-width `Ident` that never advances `i` (an infinite loop).
        let ch = rest.chars().next().unwrap();
        if is_ident_char(ch) {
            let mut j = i;
            loop {
                // One identifier component: a maximal run of ident chars.
                while j < len {
                    let cj = src[j..].chars().next().unwrap();
                    if is_ident_char(cj) {
                        j += cj.len_utf8();
                    } else {
                        break;
                    }
                }
                // Long identifiers are `ident("."ident)*`: continue across a
                // `.` ONLY when an ident char follows (`Thm.add_oracle`,
                // `HOL.List`). A trailing `.`/`..` NOT followed by an ident
                // char is a glued proof terminal (`frac_neg_frac..`,
                // `TrueI.`), NOT part of the word — leave it for the dot-run
                // `Sym` arm below so the goal-stack tracker and the keyword
                // table see the `.`/`..` terminal exactly.
                if j < len
                    && bytes[j] == b'.'
                    && src[j + 1..].chars().next().is_some_and(is_ident_char)
                {
                    j += 1;
                    continue;
                }
                break;
            }
            tokens.push(Token { kind: TokenKind::Ident, start: i, end: j });
            i = j;
            continue;
        }

        // Otherwise: a run of symbolic characters (operators, punctuation,
        // braces). Stop at whitespace, ident chars, or any delimiter opener so
        // those are handled by their own arms next iteration. Braces `{`/`}`
        // are cut as single-char tokens and the proof terminals `.`/`..` as a
        // maximal DOT run, so the keyword table can see `{`/`}`/`.`/`..`
        // exactly (a dot run reaches here either standalone or split off a
        // glued `ident..` by the long-identifier rule above).
        if ch == '{' || ch == '}' {
            tokens.push(Token { kind: TokenKind::Sym, start: i, end: i + 1 });
            i += 1;
            continue;
        }
        if ch == '.' {
            let mut j = i;
            while j < len && bytes[j] == b'.' {
                j += 1;
            }
            tokens.push(Token { kind: TokenKind::Sym, start: i, end: j });
            i = j;
            continue;
        }
        let mut j = i;
        while j < len {
            let cj = src[j..].chars().next().unwrap();
            if cj.is_whitespace()
                || is_ident_char(cj)
                || cj == '"'
                || cj == '`'
                || cj == '{'
                || cj == '}'
                || cj == '.'
                || cj == OPEN_UNICODE
                || cj == CLOSE_UNICODE
                || src[j..].starts_with("(*")
                || src[j..].starts_with("\\<")
            {
                break;
            }
            j += cj.len_utf8();
        }
        if j == i {
            // Defensive: never stall (e.g. a lone `›`). Consume one char.
            j = i + ch.len_utf8();
        }
        tokens.push(Token { kind: TokenKind::Sym, start: i, end: j });
        i = j;
    }

    tokens
}

/// Scan a `(* … *)` comment starting at `start` (which points at the `(`),
/// honoring NESTING. Returns the byte offset just past the matching `*)`. An
/// unterminated comment consumes to EOF.
fn scan_nested_comment(src: &str, start: usize) -> usize {
    let mut depth = 0usize;
    let mut i = start;
    let len = src.len();
    while i < len {
        if src[i..].starts_with("(*") {
            depth += 1;
            i += 2;
        } else if src[i..].starts_with("*)") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += next_char_len(src, i);
        }
    }
    len
}

/// Scan a cartouche starting at `start`. Honors NESTING and BOTH delimiter
/// spellings interchangeably (Unicode `‹`/`›` and ASCII `\<open>`/`\<close>`)
/// — a worker may freely mix them. Returns the offset just past the matching
/// close. Unterminated ⇒ EOF.
fn scan_cartouche(src: &str, start: usize) -> usize {
    let mut depth = 0usize;
    let mut i = start;
    let len = src.len();
    while i < len {
        if src[i..].starts_with(OPEN_ASCII) {
            depth += 1;
            i += OPEN_ASCII.len();
        } else if src[i..].starts_with(CLOSE_ASCII) {
            depth -= 1;
            i += CLOSE_ASCII.len();
            if depth == 0 {
                return i;
            }
        } else if src[i..].starts_with(OPEN_UNICODE) {
            depth += 1;
            i += OPEN_UNICODE.len_utf8();
        } else if src[i..].starts_with(CLOSE_UNICODE) {
            depth -= 1;
            i += CLOSE_UNICODE.len_utf8();
            if depth == 0 {
                return i;
            }
        } else {
            i += next_char_len(src, i);
        }
    }
    len
}

/// Scan a quoted region (`"…"` or `` `…` ``) starting at `start` (the opening
/// quote), honoring backslash escapes. Returns the offset just past the
/// closing quote. Unterminated ⇒ EOF.
fn scan_quoted(src: &str, start: usize, quote: char) -> usize {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut i = start + quote.len_utf8();
    while i < len {
        let c = src[i..].chars().next().unwrap();
        if c == '\\' {
            // Skip the escaped char.
            i += 1;
            if i < len {
                i += next_char_len(src, i);
            }
        } else if c == quote {
            return i + quote.len_utf8();
        } else {
            i += c.len_utf8();
        }
    }
    len
}

/// Scan forward from `start` until `needle` is found; return the offset just
/// past `needle`. Not found ⇒ EOF.
fn scan_until(src: &str, start: usize, needle: &str) -> usize {
    match src[start..].find(needle) {
        Some(rel) => start + rel + needle.len(),
        None => src.len(),
    }
}

/// UTF-8 width of the char at byte `i` (≥1; defensive 1 if mid-codepoint).
fn next_char_len(src: &str, i: usize) -> usize {
    src[i..].chars().next().map(char::len_utf8).unwrap_or(1)
}

// ===========================================================================
// Keyword-kind table (hardcoded minimal HOL set; classify by KIND not name)
// ===========================================================================
//
// The increment-1 stand-in for the dumped per-session keyword table (research
// Part B.5 / D.2). The checker increment replaces this with the real
// `Thy_Header.get_keywords` dump keyed to the session+AFP pin, and adds the
// server cross-check at acceptance. Classified by KIND — name lists rot, kinds
// are stable (`fun`/`datatype` discharge obligations internally ⇒ thy_decl; a
// goal command opens a proof ⇒ theory_goal).

/// `is_theory_goal` commands: the statement OPENS a proof. The principal
/// declaration of a proof-bearing node.
const THEORY_GOAL_KEYWORDS: &[&str] = &[
    "theorem",
    "lemma",
    "corollary",
    "proposition",
    "schematic_goal",
];

/// `thy_decl*` commands: a theory declaration that does NOT open an Isar proof
/// (`fun`/`datatype`/`primrec` discharge obligations internally). The
/// principal declaration of a definition node. `axiomatization` is present
/// ONLY to be RECOGNIZED and REJECTED (it asserts arbitrary axioms).
const THY_DECL_KEYWORDS: &[&str] = &[
    "definition",
    "abbreviation",
    "type_synonym",
    "datatype",
    "fun",
    "primrec",
    "record",
    "declare",
    "lemmas",
    "axiomatization",
];

/// Proof-part starter keywords: the first such token after a theory-goal
/// statement marks the statement's END (the proof begins). Note `using` and
/// `unfolding` are PROOF (they augment facts for the subsequent step), so the
/// statement/body boundary is BEFORE a leading `using` — the direct analogue
/// of "everything before `-- BODY`". `sorry`/`oops` are open/abandon markers.
const PROOF_PART_KEYWORDS: &[&str] = &[
    "proof",
    "apply",
    "by",
    "..",
    ".",
    "done",
    "sorry",
    "oops",
    "using",
    "unfolding",
    "including",
    "supply",
    "have",
    "show",
    "hence",
    "thus",
    "next",
    "qed",
    "{",
    "}",
];

/// True iff `word` (an outer `Ident` / single-char `Sym`) opens a proof-bearing
/// statement.
fn is_theory_goal(word: &str) -> bool {
    THEORY_GOAL_KEYWORDS.contains(&word)
}

/// True iff `word` is a non-proof theory declaration command.
fn is_thy_decl(word: &str) -> bool {
    THY_DECL_KEYWORDS.contains(&word)
}

/// True iff `word` is a principal command keyword (goal OR decl) — the start
/// of a node's principal command span.
fn is_principal_command(word: &str) -> bool {
    is_theory_goal(word) || is_thy_decl(word)
}

/// True iff `word` is a proof-part starter (marks a statement's end).
fn is_proof_part(word: &str) -> bool {
    PROOF_PART_KEYWORDS.contains(&word)
}

/// Theory-context commands that are NEVER legal anywhere in a Tablet node
/// theory (proof-only-skip tranche 2). Each of these is a THEORY-level
/// command that changes how DOWNSTREAM theories elaborate (notation /
/// translation / bundle / locale-interpretation / name-space hiding /
/// context blocks) while being invisible to `signature_hash` (which covers
/// only the principal-command span) — so one of them placed between the
/// proof end and `end`, or between `begin` and the principal declaration,
/// would let a "proof-only" delta silently change downstream name
/// resolution. None of them is legal Isar proof syntax and none is legal
/// inside a statement, so an exact bare-token ban over the whole file has
/// no false positives (payloads — strings/cartouches/comments — are opaque
/// tokens and can never trip a keyword scan).
///
/// This is deliberately a shape-gate-local list, NOT an addition to the
/// descriptor's `ISABELLE_HOL_FORBIDDEN_KEYWORDS` (the lexical soundness
/// policy consumed by `forbidden_keyword_line_hits` and worker advisories):
/// these commands are not a soundness/oracle surface, they are a
/// downstream-elaboration surface, and only `validate_node_shape` needs
/// them.
const THEORY_CONTEXT_COMMANDS: &[&str] = &[
    "notation",
    "no_notation",
    "type_notation",
    "translations",
    "bundle",
    "unbundle",
    "interpretation",
    "locale",
    "hide_const",
    "hide_fact",
    "hide_type",
    "context",
];

/// Cert-diagnostic commands the SERVER injects into its probe theory and a
/// node theory may never contain (`validate_node_shape` bans them alongside
/// the `print_*` prefix; `forbidden_keyword_line_hits` surfaces them).
/// Module-level so [`consume_trailing_methods`]' known-command exclusion can
/// reuse the same list.
const CERT_DIAGNOSTIC_COMMANDS: &[&str] = &["thm_oracles", "thm_deps", "thm"];

/// Bare ZERO-ARGUMENT diagnostic commands (Pure `diag` keywords that take no
/// argument token at all). Every ELABORATION-AFFECTING theory command carries
/// at least one argument token (a name / target / expression), so a smuggled
/// multi-token command always leaves residue for the only-comments post-proof
/// rule; these are the only bare commands a single-token swallow in
/// [`consume_trailing_methods`] could hide, so they are excluded there
/// explicitly (`print_*` and the cert diagnostics are excluded by their own
/// lists).
const BARE_DIAGNOSTIC_COMMANDS: &[&str] = &["unused_thms", "welcome"];

/// Isar commands that OPEN a nested goal inside a proof (each is closed by
/// its own terminal proof: `by …`, `.`, `..`, `sorry`, `done`, or a
/// `proof … qed` block). Used by [`post_principal_region_start`]'s
/// goal-stack tracker. `subgoal` is here too: it converts the first
/// pending subgoal into a nested goal proved exactly like the others.
const ISAR_GOAL_OPENERS: &[&str] = &[
    "have",
    "show",
    "hence",
    "thus",
    "obtain",
    "consider",
    "guess",
    "interpret",
    "subgoal",
];

/// The text of token `t` over `src`.
fn token_text<'a>(src: &'a str, t: &Token) -> &'a str {
    &src[t.start..t.end]
}

/// True iff token `t` is a *command-keyword candidate*: an `Ident` or a
/// single-char brace `Sym` (`{`/`}`). Only these can be classified by the
/// keyword table — a string/cartouche/comment payload or a symbolic run never
/// is. This is the "scan COMMAND-KEYWORD tokens, not raw text" rule that makes
/// the forbidden-command / open / boundary checks hazard-proof.
fn is_keyword_token(src: &str, t: &Token) -> bool {
    match t.kind {
        TokenKind::Ident => true,
        TokenKind::Sym => {
            let s = token_text(src, t);
            s == "{" || s == "}" || s == "." || s == ".."
        }
        _ => false,
    }
}

// ===========================================================================
// split
// ===========================================================================

/// Locate the principal command span and the statement/proof boundary for one
/// node `.thy`, returning a [`FilespecSplit`] (reused so both
/// `SourceModel::split` arms stay single-typed). The boundary bytes:
///
/// * `body_marker_start_byte` = `body_marker_end_byte` = the **proof-start
///   byte** (the first proof-part token after the principal goal command, or —
///   for a `thy_decl` node with no Isar proof — the end of the principal
///   command span). There is no marker LINE in Isabelle, so the two coincide:
///   `content[..start]` is everything up to the proof and `content[start..]`
///   is the proof body, mirroring the Lean `[..body]` / `[body_end..]` split.
///
/// Errors if no principal (`is_theory_goal`/`thy_decl`) command introducing a
/// name is found after `theory … begin`.
pub fn split(src: &str, node: &str) -> Result<FilespecSplit, String> {
    let tokens = tokenize(src);
    let (cmd_idx, _name) = principal_command(src, &tokens, node)?;

    // The principal command keyword token.
    let cmd_tok = tokens[cmd_idx];
    let goal = is_theory_goal(token_text(src, &cmd_tok));

    // Find the boundary: the first proof-part token strictly after the command
    // keyword (for a goal), else the start of the NEXT principal command (or
    // EOF) for a non-proof decl.
    let boundary = if goal {
        tokens[cmd_idx + 1..]
            .iter()
            .find(|t| is_keyword_token(src, t) && is_proof_part(token_text(src, t)))
            .map(|t| t.start)
            // A goal with no proof token at all (malformed/open header) ⇒ the
            // boundary is EOF; the whole tail is the (empty) body.
            .unwrap_or(src.len())
    } else {
        // thy_decl: the span runs until the next principal command keyword, a
        // proof-part token (a `fun`/`function`-style obligation), the `end`
        // theory terminator, or EOF — none of which belong to the definition.
        tokens[cmd_idx + 1..]
            .iter()
            .find(|t| {
                is_keyword_token(src, t)
                    && (is_principal_command(token_text(src, t))
                        || is_proof_part(token_text(src, t))
                        || token_text(src, t) == "end")
            })
            .map(|t| t.start)
            .unwrap_or(src.len())
    };

    let source_hash = sha256_hex(src.as_bytes());
    Ok(FilespecSplit {
        node: NodeId::from(node),
        body_marker_start_byte: boundary,
        body_marker_end_byte: boundary,
        source_hash,
        record_version: crate::filespec_split::RECORD_VERSION,
    })
}

/// Find the principal command: the FIRST `is_theory_goal`/`thy_decl` command
/// token that occurs after the `theory … begin` header and introduces the name
/// `node`. Returns `(token_index, declared_name)`.
///
/// The declared name is the first `Ident` token after the command keyword,
/// with a trailing `:` / type-annotation stripped (Isabelle names match the
/// node-id shape, so we take the leading ident and drop a `::`/`:` suffix).
fn principal_command(
    src: &str,
    tokens: &[Token],
    node: &str,
) -> Result<(usize, String), String> {
    let begin_idx = find_begin(src, tokens);
    let scan_from = begin_idx.map(|b| b + 1).unwrap_or(0);

    for (offset, t) in tokens[scan_from..].iter().enumerate() {
        if !is_keyword_token(src, t) {
            continue;
        }
        let word = token_text(src, t);
        if is_principal_command(word) {
            let idx = scan_from + offset;
            if let Some(name) = declared_name(src, tokens, idx) {
                if name == node {
                    return Ok((idx, name));
                }
            }
        }
    }
    Err(format!(
        "isabelle_filespec: no principal command (theorem/lemma/definition/…) named `{node}` found after `theory … begin`"
    ))
}

/// The `begin` token index, if present (the header terminator). Used to skip
/// the `theory … imports … begin` header when locating the principal command.
fn find_begin(src: &str, tokens: &[Token]) -> Option<usize> {
    tokens
        .iter()
        .position(|t| t.kind == TokenKind::Ident && token_text(src, t) == "begin")
}

/// The name a principal command at token `cmd_idx` introduces: the next `Ident`
/// token, with any trailing `:`/`::` annotation stripped. `None` if no ident
/// follows before a proof-part / next command.
fn declared_name(src: &str, tokens: &[Token], cmd_idx: usize) -> Option<String> {
    for t in &tokens[cmd_idx + 1..] {
        match t.kind {
            TokenKind::Ident => {
                let raw = token_text(src, t);
                // Strip a trailing `:`/`::` if the lexer glued it (it won't,
                // since `:` is a Sym, but be defensive) and any dotted suffix
                // is kept as-is (Isabelle long names are legal but a node name
                // is a bare ident).
                let name = raw.trim_end_matches(':');
                if name.is_empty() {
                    continue;
                }
                return Some(name.to_string());
            }
            // A symbolic token (`:`, `::`) between the keyword and the name is
            // unusual but skip it; a proof-part / brace ⇒ give up.
            TokenKind::Sym => {
                let s = token_text(src, t);
                if s == "{" || s == "}" {
                    return None;
                }
                continue;
            }
            // A comment/string before the name ⇒ skip (opaque).
            _ => continue,
        }
    }
    None
}

// ===========================================================================
// signature_hash
// ===========================================================================

/// The Tier-1 signature hash: SHA-256 (lowercase hex, matching
/// `filespec_split::sha256_hex`) over the NORMALIZED principal-command span.
///
/// Normalization (research D.3):
/// * The region is the principal command span `[cmd_start .. proof_start]` —
///   the goal/decl command up to the first proof-part token. The `theory …
///   begin` header, imports, and the proof body are all OUTSIDE the region, so
///   adding an import / editing the proof does not move the hash.
/// * Symbol-SPELLING canonicalization: every Unicode cartouche delimiter and
///   connective is rewritten to its ASCII `\<name>` spelling (`‹`→`\<open>`,
///   `∀`→`\<forall>`, …) — byte-safe everywhere (it is exactly Isabelle's
///   symbol layer), so `∀` and `\<forall>` hash identically.
/// * Outer (code-region) whitespace collapses to single spaces; comments drop
///   to a single space. String/alt-string/cartouche/verbatim payloads are kept
///   BYTE-EXACT (no inner-syntax term parse — that is Tier 2's job), modulo the
///   spelling canonicalization applied uniformly first.
///
/// Drift: stable across proof-body + header/import edits and `∀`↔`\<forall>` /
/// `‹›`↔`\<open>\<close>` rewrites; changes on a statement edit
/// (`shows "P"`→`shows "Q"`). `_repo_path` is accepted for signature symmetry
/// with the Lean arm and is unused (pure text).
pub fn signature_hash(_repo_path: &Path, src: &str, node: &str) -> Result<String, String> {
    let split = split(src, node)?;
    let tokens = tokenize(src);
    let (cmd_idx, _name) = principal_command(src, &tokens, node)?;
    let cmd_start = tokens[cmd_idx].start;
    let region_end = split.body_marker_start_byte;
    if cmd_start >= region_end {
        // Degenerate (no signature bytes) — hash the empty normalization.
        return Ok(sha256_hex(b""));
    }
    let region = &src[cmd_start..region_end];
    let normalized = normalize_signature(region);
    Ok(sha256_hex(normalized.as_bytes()))
}

/// Normalize a principal-command-span slice for hashing: spelling-canonicalize,
/// then collapse outer whitespace / drop comments while preserving payloads.
fn normalize_signature(region: &str) -> String {
    // Step 1: re-tokenize the region itself so we know payload boundaries.
    let toks = tokenize(region);
    let mut out = String::new();
    let mut need_space = false;
    for t in &toks {
        let text = token_text(region, t);
        match t.kind {
            // Comments contribute nothing (a comment edit is not a statement
            // edit); they act as a token separator only.
            TokenKind::Comment => {
                need_space = true;
            }
            // Payload-bearing tokens: keep byte-exact, but apply the byte-safe
            // symbol-spelling canonicalization (a `‹`/`∀` inside a string is
            // the same symbol as its `\<name>` spelling to Isabelle's lexer).
            TokenKind::String
            | TokenKind::AltString
            | TokenKind::Cartouche
            | TokenKind::Verbatim => {
                if need_space && !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&canonicalize_symbol_spellings(text));
                need_space = true;
            }
            // Ident / Sym: canonicalize spellings, single-space-separate.
            TokenKind::Ident | TokenKind::Sym => {
                if need_space && !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&canonicalize_symbol_spellings(text));
                need_space = true;
            }
        }
    }
    out.trim().to_string()
}

/// Bounded, documented connective + delimiter spelling table. Maps each
/// Unicode symbol Isabelle accepts (that we canonicalize in increment 1) to its
/// ASCII `\<name>` form. The cartouche delimiters are structurally certain; the
/// connectives are a bounded set (the full `etc/symbols` dump is deferred to
/// the checker increment — research risk 2). Canonical form is the ASCII
/// spelling (every Unicode symbol HAS one; the reverse is not 1:1 spell-safe).
const SYMBOL_CANON: &[(char, &str)] = &[
    // Cartouche delimiters (structurally certain).
    ('\u{2039}', "\\<open>"),   // ‹
    ('\u{203A}', "\\<close>"),  // ›
    // Logical connectives / quantifiers (bounded documented set).
    ('\u{2200}', "\\<forall>"),     // ∀
    ('\u{2203}', "\\<exists>"),     // ∃
    ('\u{00AC}', "\\<not>"),        // ¬
    ('\u{2227}', "\\<and>"),        // ∧
    ('\u{2228}', "\\<or>"),         // ∨
    ('\u{27F6}', "\\<longrightarrow>"), // ⟶
    ('\u{27F9}', "\\<Longrightarrow>"), // ⟹
    ('\u{21D2}', "\\<Rightarrow>"), // ⇒
    ('\u{2192}', "\\<rightarrow>"), // →
    ('\u{27F7}', "\\<longleftrightarrow>"), // ⟷
    ('\u{2194}', "\\<leftrightarrow>"), // ↔
    ('\u{2208}', "\\<in>"),         // ∈
    ('\u{2286}', "\\<subseteq>"),   // ⊆
    ('\u{222A}', "\\<union>"),      // ∪
    ('\u{2229}', "\\<inter>"),      // ∩
    ('\u{2260}', "\\<noteq>"),      // ≠
    ('\u{2264}', "\\<le>"),         // ≤
    ('\u{2265}', "\\<ge>"),         // ≥
    ('\u{03BB}', "\\<lambda>"),     // λ
];

/// Rewrite every Unicode symbol in `SYMBOL_CANON` to its ASCII `\<name>`
/// spelling. Byte-safe in any context (Isabelle treats `∀` and `\<forall>` as
/// the identical symbol token). Idempotent: applying it to text already in
/// ASCII form is a no-op.
fn canonicalize_symbol_spellings(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match SYMBOL_CANON.iter().find(|(u, _)| *u == c) {
            Some((_, ascii)) => out.push_str(ascii),
            None => out.push(c),
        }
    }
    out
}

// ===========================================================================
// declaration_kind
// ===========================================================================

/// Classify node `node`'s principal command as `"definition"` (a `thy_decl`
/// command — `definition`/`fun`/`datatype`/… ∈ [`THY_DECL_KEYWORDS`], the
/// non-proof-bearing declarations) or `"theorem_like"` (a theory-goal command —
/// `theorem`/`lemma`/… ∈ [`THEORY_GOAL_KEYWORDS`], which open an Isar proof).
/// Returns `""` when no principal command named `node` is found.
///
/// The Isabelle analogue of `runtime_cli_observations::declaration_kind` (the
/// Lean declaration-head ⇒ `is_definition_kind` ? "definition" : "theorem_like"
/// scan): the proof-bearing / definitional split keyed off the principal
/// command's keyword. Surfaced as [`crate::backend::SourceModel::declaration_kind`]
/// and consumed by `evaluate_node_observation`'s declaration-kind ↔ tex-
/// environment consistency checks. `is_principal_command` guarantees the
/// resolved command is one of the two classes.
pub fn declaration_kind(src: &str, node: &str) -> String {
    let tokens = tokenize(src);
    let Ok((cmd_idx, _name)) = principal_command(src, &tokens, node) else {
        return String::new();
    };
    let word = token_text(src, &tokens[cmd_idx]);
    if is_thy_decl(word) {
        "definition".to_string()
    } else if is_theory_goal(word) {
        "theorem_like".to_string()
    } else {
        // Unreachable: `principal_command` only returns `is_principal_command`
        // tokens (= goal OR decl). Defensive empty for forward safety.
        String::new()
    }
}

// ===========================================================================
// post-principal region (proof-only-skip tranche 2 shape tightening)
// ===========================================================================

/// One frame of [`post_principal_region_start`]'s Isar structure stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProofFrame {
    /// An open goal awaiting its proof (the principal goal, a nested
    /// `have`/`show`/…, or a `subgoal`).
    Goal,
    /// A structured `proof … qed` block.
    Block,
}

/// Locate the first token index AFTER the principal declaration concludes.
///
/// * For a `thy_decl` principal (definition nodes) that is the end of the
///   principal command span — the same boundary [`split`] computes: the
///   first principal-command / proof-part / `end` keyword token after the
///   command.
/// * For a theory-goal principal it is the end of the top-level proof,
///   INCLUDING any trailing method arguments of the closing `qed`/`by`
///   (`qed auto`, `by (induct n) auto`). Located by a small Isar
///   goal-stack tracker: `proof` pushes a block, goal openers
///   ([`ISAR_GOAL_OPENERS`]) push goals, terminal proofs
///   (`by`/`.`/`..`/`sorry`/`done`/`\<proof>`/`oops`) pop the innermost
///   goal, `qed` pops its block plus the goal it proves. When the
///   PRINCIPAL goal pops, the proof has concluded.
///
/// `Ok(None)`: the proof never concludes before EOF (an unfinished
/// apply-script / malformed tail). There is then no post-proof region to
/// check. [`validate_node_shape`] fails CLOSED on this for a CLOSED node
/// (no `sorry`/`\<proof>` marker): an unlocatable conclusion leaves the
/// post-proof region unpoliced, so it must be a shape error rather than a
/// silent pass; an OPEN node keeps the lenient behaviour (it concludes via
/// its `sorry` / `\<proof>` marker, and an unfinished open tail is
/// legitimate mid-work). `Err`: the structure is untrackable (e.g. a `qed`
/// with no open `proof` block); the caller fails closed with a shape error.
fn post_principal_region_start(
    src: &str,
    tokens: &[Token],
    cmd_idx: usize,
) -> Result<Option<usize>, String> {
    let cmd_word = token_text(src, &tokens[cmd_idx]);
    if is_thy_decl(cmd_word) {
        // Mirror `split`'s thy_decl boundary, but return the TOKEN index.
        for (offset, t) in tokens[cmd_idx + 1..].iter().enumerate() {
            if is_keyword_token(src, t) {
                let word = token_text(src, t);
                if is_principal_command(word) || is_proof_part(word) || word == "end" {
                    return Ok(Some(cmd_idx + 1 + offset));
                }
            }
        }
        return Ok(None);
    }

    // Theory goal: start tracking at the first proof-part token (the
    // statement/proof boundary `split` uses). No proof token at all ⇒ no
    // post-proof region (the malformed tail is the statement's problem).
    let Some(first_proof_offset) = tokens[cmd_idx + 1..]
        .iter()
        .position(|t| is_keyword_token(src, t) && is_proof_part(token_text(src, t)))
    else {
        return Ok(None);
    };
    let mut stack: Vec<ProofFrame> = vec![ProofFrame::Goal];
    let mut i = cmd_idx + 1 + first_proof_offset;
    while i < tokens.len() {
        let t = &tokens[i];
        let word = match t.kind {
            TokenKind::Ident => token_text(src, t),
            TokenKind::Sym => {
                let s = token_text(src, t);
                if s == "." || s == ".." || s == "\\<proof>" {
                    s
                } else {
                    // Braces / arbitrary symbol runs: structurally neutral.
                    i += 1;
                    continue;
                }
            }
            // Payloads (comment/string/cartouche/verbatim): opaque.
            _ => {
                i += 1;
                continue;
            }
        };
        match word {
            "proof" => stack.push(ProofFrame::Block),
            "qed" => match stack.pop() {
                Some(ProofFrame::Block) => {
                    if stack.last() == Some(&ProofFrame::Goal) {
                        stack.pop();
                    }
                    if stack.is_empty() {
                        // `qed` may carry ONE trailing method argument.
                        return Ok(Some(consume_trailing_methods(src, tokens, i + 1, 1)));
                    }
                }
                _ => {
                    return Err(
                        "`qed` with no open `proof` block in the principal proof".to_string()
                    );
                }
            },
            "by" => {
                if stack.last() == Some(&ProofFrame::Goal) {
                    stack.pop();
                    if stack.is_empty() {
                        // `by` may carry up to TWO trailing method arguments.
                        return Ok(Some(consume_trailing_methods(src, tokens, i + 1, 2)));
                    }
                }
                // Top is a Block: terminal of a nested goal this tracker did
                // not model as an opener — structurally neutral (the missed
                // opener and its terminal cancel).
            }
            "." | ".." | "sorry" | "done" | "\\<proof>" | "oops" => {
                if stack.last() == Some(&ProofFrame::Goal) {
                    stack.pop();
                    if stack.is_empty() {
                        return Ok(Some(i + 1));
                    }
                }
            }
            _ if ISAR_GOAL_OPENERS.contains(&word) => stack.push(ProofFrame::Goal),
            _ => {}
        }
        i += 1;
    }
    Ok(None)
}

/// True iff `word` is a KNOWN COMMAND (from any table this module holds) and
/// therefore can never be a trailing proof-method name: the theory terminator
/// `end`, the theory-context set, principal commands, proof-part starters,
/// the descriptor's forbidden commands, the cert diagnostics + `print_*`
/// prefix, and the bare zero-argument diagnostics. Used by
/// [`consume_trailing_methods`] so `qed <command>` never swallows a command
/// as a fake method. No REAL single-ident method (`auto`, `simp`, `blast`,
/// `standard`, …) is in any of these tables (`eval`/`normalization`/
/// `code_simp`/`tactic` ARE — but those are globally banned methods, so
/// breaking early only re-reports an already-rejected file).
fn is_known_non_method_command(word: &str) -> bool {
    word == "end"
        || THEORY_CONTEXT_COMMANDS.contains(&word)
        || is_principal_command(word)
        || is_proof_part(word)
        || crate::backend::ISABELLE_HOL_FORBIDDEN_KEYWORDS.contains(&word)
        || CERT_DIAGNOSTIC_COMMANDS.contains(&word)
        || word.starts_with("print_")
        || BARE_DIAGNOSTIC_COMMANDS.contains(&word)
}

/// Consume up to `max_methods` trailing method expressions starting at token
/// index `i` (the token after a concluding `qed`/`by`), returning the index
/// of the first token past them. A method expression at the outer-token
/// level is either a single identifier (`auto`, `simp_all`) or a
/// parenthesized group (`(simp add: foo)` — balanced by counting
/// parenthesis CHARS across `Sym` runs), optionally followed by `+`/`?`
/// combinator symbols. A single identifier is consumed ONLY when it is not a
/// known command ([`is_known_non_method_command`]), so `qed end`,
/// `qed unbundle …`, and `qed unused_thms` all leave the command token in
/// the post-proof region for the only-comments rule to reject. This closes
/// the fake-method swallow: a multi-token command (`qed notation foo (…)`)
/// leaves its remaining tokens in the region anyway (every
/// elaboration-affecting Isabelle command carries at least one argument
/// token), and the residual bare zero-argument commands are diagnostics,
/// all of which the exclusion lists name.
fn consume_trailing_methods(src: &str, tokens: &[Token], mut i: usize, max_methods: usize) -> usize {
    for _ in 0..max_methods {
        // Comments are legal anywhere and act as separators.
        while tokens.get(i).is_some_and(|t| t.kind == TokenKind::Comment) {
            i += 1;
        }
        let Some(t) = tokens.get(i) else { break };
        match t.kind {
            TokenKind::Ident => {
                let word = token_text(src, t);
                if is_known_non_method_command(word) {
                    break;
                }
                i += 1;
            }
            TokenKind::Sym if token_text(src, t).starts_with('(') => {
                let mut depth: i64 = 0;
                while let Some(t2) = tokens.get(i) {
                    if t2.kind == TokenKind::Sym {
                        for c in token_text(src, t2).chars() {
                            if c == '(' {
                                depth += 1;
                            } else if c == ')' {
                                depth -= 1;
                            }
                        }
                    }
                    i += 1;
                    if depth <= 0 {
                        break;
                    }
                }
            }
            _ => break,
        }
        // Postfix combinators: `+`, `?` (e.g. `by (auto)+`).
        while tokens.get(i).is_some_and(|t| {
            t.kind == TokenKind::Sym
                && !token_text(src, t).is_empty()
                && token_text(src, t).chars().all(|c| c == '+' || c == '?')
        }) {
            i += 1;
        }
    }
    i
}

/// Byte span `[start, end)` of the principal declaration's PROOF REGION, or
/// `None` when it cannot be located confidently.
///
/// * `start` is the statement/proof boundary [`split`] computes
///   (`body_marker_start_byte`): for a theory-goal principal the first
///   proof-part token after the goal command; for a `thy_decl` principal the
///   end of the principal command span.
/// * `end` is the byte where [`post_principal_region_start`]'s goal-stack
///   tracker places the first post-proof token — i.e. the span INCLUDES the
///   concluding `qed`/`by` with its trailing method arguments (and, via
///   `consume_trailing_methods`' comment-skipping, any comment interleaved
///   with those trailing methods). For a `thy_decl` principal `start == end`:
///   a definition node has NO proof region, so "file minus proof region" is
///   the whole file by construction.
///
/// `None` — fail-closed, the caller must treat the WHOLE file as significant
/// — when the principal command cannot be found, the proof never concludes
/// before EOF (`Ok(None)` from the tracker: an unfinished apply-script /
/// malformed tail), or the Isar structure is untrackable (`Err`: e.g. a
/// `qed` with no open `proof` block).
///
/// This is the tranche-3 seam for statement-region cache keying: a cone
/// member's cert-relevant identity is its file MINUS this span (header,
/// imports, statement, post-proof tail), so a proof-only edit upstream can
/// keep a downstream key stable. Policy gates (open `sorry` nodes, shape
/// validity) belong to the CALLER — this function only locates the region,
/// reusing the same tracker `validate_node_shape` trusts.
pub fn principal_proof_region(src: &str, node: &str) -> Option<(usize, usize)> {
    let tokens = tokenize(src);
    let (cmd_idx, _name) = principal_command(src, &tokens, node).ok()?;
    let end_idx = match post_principal_region_start(src, &tokens, cmd_idx) {
        Ok(Some(idx)) => idx,
        // Non-concluding proof / untrackable structure: no confident region.
        Ok(None) | Err(_) => return None,
    };
    // Token starts are char boundaries; `end_idx == tokens.len()` ⇒ EOF.
    let end_byte = tokens.get(end_idx).map(|t| t.start).unwrap_or(src.len());
    let start_byte = split(src, node).ok()?.body_marker_start_byte;
    Some((start_byte.min(end_byte), end_byte))
}

// ===========================================================================
// validate_node_shape
// ===========================================================================

/// Validate one node `.thy`'s shape, returning human-readable errors (empty =
/// valid). Mirrors `filespec::validate_lean_node_shape`. Enforced (research
/// D.1 / risk 3 / risk 7):
///
/// * NO `keywords` clause in the header (would extend the keyword table and
///   defeat the static-table scan).
/// * NO command-defining / ML / oracle / axiom command anywhere (scanned by
///   COMMAND-KEYWORD tokens, not raw text, from the descriptor's
///   `forbidden_keywords`). `oops` ⇒ a SPECIFIC shape error (it abandons the
///   goal, producing no theorem — a node whose fact does not exist).
/// * EXACTLY ONE principal (`is_theory_goal`/`thy_decl`) command, and its
///   declared name is `<Node>`.
/// * NO theory-context command ([`THEORY_CONTEXT_COMMANDS`]: `notation` /
///   `translations` / `bundle` / `interpretation` / `locale` / `hide_*` /
///   `context` / …) as a bare token ANYWHERE — these change downstream
///   elaboration while being invisible to `signature_hash`. This covers, in
///   particular, the between-`begin`-and-first-declaration region, which the
///   exactly-one-principal rule does NOT police (they are not principal
///   commands).
/// * AFTER the principal declaration's proof concludes (goal nodes: the end
///   of the top-level proof incl. trailing `qed`/`by` methods; definition
///   nodes: the end of the principal command span), the ONLY tokens allowed
///   are comments/whitespace and the single closing `end`. Any other token
///   there — including commands NOT in the named list above — is a shape
///   violation: a theory-level command in that region is invisible to
///   `signature_hash` and would defeat the proof-only acceptance-skip's
///   soundness argument. The same only-comments rule applies between
///   `begin` and the principal command.
///
/// `sorry` is NOT rejected here — it is the open-proof marker (handled by
/// [`is_node_open`]); a node may legitimately be open.
pub fn validate_node_shape(src: &str, node: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let tokens = tokenize(src);

    // The `__Cert` must-fix (B2c-gate Slice 3): the checker's server-injected
    // probe theory is `Tablet_<Node>__Cert.thy`. A worker node named
    // `Foo__Cert` would COLLIDE with node `Foo`'s probe-theory path — letting
    // the worker author the very file the cert mechanism reserves. Reject any
    // node whose registered name ends in `__Cert`.
    if node.ends_with("__Cert") {
        errors.push(format!(
            "Isabelle node name `{node}` ends in `__Cert`, which is RESERVED: the checker writes a server-owned probe theory `Tablet_<Node>__Cert.thy` per node, and a node so named would collide with that reserved cert path. Rename the node."
        ));
    }

    // Collect the command-keyword tokens (Idents + classified single-char
    // Syms). Only these can be a forbidden command / principal command.
    let keyword_words: Vec<&str> = tokens
        .iter()
        .filter(|t| is_keyword_token(src, t))
        .map(|t| token_text(src, t))
        .collect();

    // The codegen-trusting proof METHODS (`eval`/`normalization`/`code_simp`):
    // present in the descriptor's forbidden set (so the scan below catches the
    // method token), but they warrant a method-specific diagnostic distinct
    // from the command-defining/ML/oracle message.
    const CODEGEN_METHODS: &[&str] = &["eval", "normalization", "code_simp"];

    // `oops` ⇒ specific shape error (abandons the goal; no theorem results).
    if keyword_words.contains(&"oops") {
        errors.push(
            "Isabelle node theory uses `oops`, which ABANDONS the goal and produces no theorem (the node's fact would not exist). Use `sorry` to leave the proof open, or finish the proof."
                .to_string(),
        );
    }

    // Forbidden commands (the descriptor's banned-COMMAND set), scanned by
    // command-keyword token so a `keywords`/`ML`/`oracle` substring inside a
    // string/cartouche/comment cannot trip it. `oops` is in this set too but
    // already gave its specific error above; skip the generic one for it.
    // `eval`/`normalization`/`code_simp` are also in the set but are proof
    // METHODS (R3) — they get the codegen-method message instead.
    let forbidden = crate::backend::ISABELLE_HOL_FORBIDDEN_KEYWORDS;
    let mut seen_forbidden: Vec<&str> = Vec::new();
    for word in &keyword_words {
        if *word == "oops" {
            continue;
        }
        if forbidden.contains(word) && !seen_forbidden.contains(word) {
            seen_forbidden.push(word);
            if CODEGEN_METHODS.contains(word) {
                errors.push(format!(
                    "Isabelle node theory uses the forbidden proof method `{word}`: `eval`/`normalization`/`code_simp` reduce the goal through the TRUSTED CODE GENERATOR (the NBE prove-False surface), which can discharge a goal without a named oracle in the trust-basis certificate. Use a kernel-checked proof method."
                ));
            } else {
                errors.push(format!(
                    "Isabelle node theory uses the forbidden command `{word}`: an ordinary Tablet node `.thy` may not author a command-defining / ML / oracle / axiom command (`keywords`/`ML`/`setup`/`oracle`/`axiomatization`/`syntax`/…). Such a command can introduce an out-of-band axiom/oracle or extend the keyword table, defeating the trust-basis certificate and the static-table outer scan."
                ));
            }
        }
    }
    // NOTE: `Thm.add_oracle` is a long ident (kept as one token by the
    // tokenizer), so the command-keyword scan above catches it directly.

    // Cert-diagnostic commands: the worker-can't-author-cert invariant requires
    // a NODE theory never run the diagnostic commands the SERVER injects into
    // the probe theory (`thm_oracles`/`thm_deps`/`thm`) or any `print_*`
    // inspection command. Scanned by command-keyword token; `print_*` is a
    // prefix match (`print_theorems`, `print_statement`, …). These are not in
    // the descriptor's lexical-policy set (they neither introduce axioms/oracles
    // nor extend the keyword table) but they ARE forbidden in a node theory.
    let mut seen_diag: Vec<&str> = Vec::new();
    for word in &keyword_words {
        let is_diag = CERT_DIAGNOSTIC_COMMANDS.contains(word) || word.starts_with("print_");
        if is_diag && !seen_diag.contains(word) {
            seen_diag.push(word);
            errors.push(format!(
                "Isabelle node theory uses the forbidden diagnostic command `{word}`: `thm`/`thm_oracles`/`thm_deps`/`print_*` are inspection commands the CHECKER injects into its server-owned probe theory; a node theory must contain only the statement + proof, never the certificate machinery."
            ));
        }
    }

    // Theory-context commands (proof-only-skip tranche 2): reject the
    // downstream-elaboration command surface as a bare token anywhere in the
    // node theory. See `THEORY_CONTEXT_COMMANDS` — these are invisible to
    // `signature_hash` yet change how downstream theories elaborate, and none
    // of them is legal statement or proof syntax, so the exact-token ban has
    // no false positives. In particular this closes the
    // between-`begin`-and-first-declaration region, which the
    // exactly-one-principal rule below does not police.
    let mut seen_theory_context: Vec<&str> = Vec::new();
    for word in &keyword_words {
        if THEORY_CONTEXT_COMMANDS.contains(word) && !seen_theory_context.contains(word) {
            seen_theory_context.push(word);
            errors.push(format!(
                "Isabelle node theory uses the theory-context command `{word}`: notation/translation/bundle/locale-interpretation/name-space-hiding/context commands change how DOWNSTREAM theories elaborate while being invisible to the node's statement signature, so an ordinary Tablet node `.thy` may never contain one. A node theory is exactly: header, the principal declaration, its proof, and the closing `end`."
            ));
        }
    }

    // Principal commands: each `is_principal_command` keyword + its declared
    // name. Enforce exactly one, named `<Node>`.
    let mut principals: Vec<(usize, Option<String>)> = Vec::new();
    let begin_idx = find_begin(src, &tokens);
    let scan_from = begin_idx.map(|b| b + 1).unwrap_or(0);
    for (offset, t) in tokens[scan_from..].iter().enumerate() {
        if is_keyword_token(src, t) && is_principal_command(token_text(src, t)) {
            let idx = scan_from + offset;
            principals.push((idx, declared_name(src, &tokens, idx)));
        }
    }

    let names: Vec<String> = principals
        .iter()
        .filter_map(|(_, n)| n.clone())
        .collect();

    if principals.is_empty() {
        errors.push(format!(
            "Isabelle node theory must contain a principal command (theorem/lemma/definition/…) named {node}; found none after `theory … begin`."
        ));
    } else if principals.len() > 1 {
        errors.push(format!(
            "Isabelle node theory must contain exactly one principal command; found {} ({:?}). Move auxiliary facts/definitions into their own registered nodes.",
            principals.len(),
            names
        ));
    } else if names.first().map(String::as_str) != Some(node) {
        errors.push(format!(
            "Isabelle node theory's principal command must be named {node}; found {:?}.",
            names.first().cloned().unwrap_or_default()
        ));
    }

    // Region rules (proof-only-skip tranche 2): outside the principal
    // declaration + its proof, ONLY comments/whitespace (and the single
    // closing `end`) are allowed. Anchored on the single principal command,
    // so these checks run only when the exactly-one rule holds (multiple /
    // missing principals already errored above and give no stable anchor).
    if principals.len() == 1 {
        let cmd_idx = principals[0].0;

        // Between `begin` and the principal command: comments only. This
        // region is invisible to `signature_hash` (which starts at the
        // principal command), so ANY command here — the named
        // theory-context set above or otherwise (`instantiation`,
        // `sublocale`, …) — could change downstream elaboration under an
        // unchanged statement signature.
        if begin_idx.is_some() {
            if let Some(t) = tokens[scan_from..cmd_idx]
                .iter()
                .find(|t| t.kind != TokenKind::Comment)
            {
                errors.push(format!(
                    "Isabelle node theory contains `{}` between `begin` and the principal declaration: only comments/whitespace are allowed there. A command in that region is invisible to the node's statement signature and can change how downstream theories elaborate.",
                    token_text(src, t)
                ));
            }
        }

        // After the principal declaration's proof concludes: comments only,
        // then the single closing `end`, then nothing.
        match post_principal_region_start(src, &tokens, cmd_idx) {
            Ok(Some(region_start)) => {
                let mut seen_end = false;
                let offender = tokens[region_start..].iter().find(|t| {
                    if t.kind == TokenKind::Comment {
                        return false;
                    }
                    if !seen_end && t.kind == TokenKind::Ident && token_text(src, t) == "end" {
                        seen_end = true;
                        return false;
                    }
                    true
                });
                if let Some(t) = offender {
                    errors.push(format!(
                        "Isabelle node theory contains `{}` after the principal declaration's proof concludes: between the end of the proof and the theory's closing `end`, only comments/whitespace are allowed. A theory-level command there (notation/translations/interpretation/hide_*/unbundle/...) changes how downstream theories elaborate while being invisible to the node's statement signature. Move the proof-relevant part into the proof and delete the rest.",
                        token_text(src, t)
                    ));
                }
            }
            // No post-proof region exists: the tracker could not locate
            // where the principal declaration concludes before EOF. For an
            // OPEN node this is legitimate (an open proof concludes via its
            // `sorry`/`\<proof>` marker; an unfinished tail is the open
            // proof's business) — keep today's behaviour. For a CLOSED node
            // this arm must FAIL CLOSED, mirroring the tranche-3 caller
            // (`isabelle_cone_member_statement_hash` falls back to full
            // bytes on an unlocatable region): a closed node whose principal
            // proof the tracker cannot see conclude leaves the post-proof
            // region unpoliced, so a theory-context command there —
            // invisible to `signature_hash` — would ride through the shape
            // gate and the proof-only acceptance skip.
            Ok(None) => {
                if !is_node_open(src) {
                    errors.push(
                        "Isabelle node theory's principal proof does not conclude trackably: the goal-stack tracker reached end-of-file without seeing the principal declaration's proof conclude, so the shape gate cannot verify that nothing follows the proof, and fails closed for a closed node. Finish the proof with a standard `proof … qed`, terminal `by`/`.`/`..`, or apply-script `… done` conclusion (followed by the theory's closing `end`), or mark the proof open with `sorry`."
                            .to_string(),
                    );
                }
            }
            // Untrackable structure: fail closed with a shape error.
            Err(reason) => {
                errors.push(format!(
                    "Isabelle node theory's proof structure could not be tracked to its conclusion ({reason}); the shape gate cannot verify that nothing follows the principal proof, and fails closed. Restructure the proof into a standard `proof … qed`, terminal `by`/`.`/`..`, apply-script `… done`, or `sorry` form."
                ));
            }
        }
    }

    errors
}

// ===========================================================================
// is_node_open
// ===========================================================================

/// Cheap textual "is this node still open?" — the Isabelle analogue of
/// `worker_normalization::has_sorry` (research risk 7 / §2.3). Tokenize (which
/// masks comments/strings/cartouches), then scan COMMAND-KEYWORD tokens for an
/// OPEN marker: `sorry` (the skip-proof oracle) or the raw-proof placeholder
/// `\<proof>`. A `sorry` inside a comment/string/cartouche ⇒ NOT open (it is a
/// payload, never a keyword token). `oops` ⇒ NOT "open" (it abandons the goal —
/// a shape error caught by [`validate_node_shape`], not an open proof).
///
/// Feeds `open_nodes_from_repo` → the contract's `openNodes` → phase advance.
pub fn is_node_open(src: &str) -> bool {
    let tokens = tokenize(src);
    for t in &tokens {
        if t.kind == TokenKind::Ident && token_text(src, t) == "sorry" {
            return true;
        }
        // `\<proof>` lexes as a single `Sym` token (a `\<name>` symbol).
        if t.kind == TokenKind::Sym && token_text(src, t) == "\\<proof>" {
            return true;
        }
    }
    false
}

// ===========================================================================
// tablet_imports
// ===========================================================================

/// Intra-tablet import edges parsed from the `imports` clause: a
/// `Tablet_<Dep>` theory name ⇒ the bare `<Dep>` node id (research D.5 /
/// risk 5). Mirrors `worker_normalization::extract_tablet_imports`
/// (`import Tablet.X` ⇒ `X`); the `dep != node` self-filter stays at the
/// caller.
///
/// Scans the `imports` clause specifically: the `Ident` tokens between the
/// `imports` keyword and the `begin`/`keywords` terminator. A `Tablet_`-
/// prefixed name there yields its suffix.
pub fn tablet_imports(src: &str) -> Vec<NodeId> {
    let tokens = tokenize(src);
    let mut out = Vec::new();

    // Find the `imports` keyword; collect idents until `begin`/`keywords`.
    let mut in_imports = false;
    for t in &tokens {
        if t.kind != TokenKind::Ident {
            // A non-ident (string/sym/comment) inside the imports clause is
            // not an import name; keep scanning.
            continue;
        }
        let word = token_text(src, t);
        if !in_imports {
            if word == "imports" {
                in_imports = true;
            }
            continue;
        }
        // In the imports clause.
        if word == "begin" || word == "keywords" {
            break;
        }
        if let Some(dep) = word.strip_prefix("Tablet_") {
            if !dep.is_empty() {
                out.push(NodeId::from(dep));
            }
        }
    }

    out
}

// ===========================================================================
// imports_clause_idents
// ===========================================================================

/// EVERY `Ident` token inside the `imports` clause (between the `imports`
/// keyword and its `begin`/`keywords` terminator) — intra-tablet
/// `Tablet_<Dep>` names AND library theories (`Main`, `HOL-Library.Foo`
/// spelled bare) alike, in source order.
///
/// [`tablet_imports`] deliberately narrows to the `Tablet_` prefix (the
/// dep-graph edge walk); the proof-only acceptance-skip needs the FULL list,
/// because adding/removing a LIBRARY import also changes downstream name
/// resolution (everything a node imports is transitively visible to its
/// importers). Consumed by the delta step's proof-only predicate
/// (`runtime_cli_observations::isabelle_proof_only_delta_skip`), compared
/// order-insensitively. Quoted / non-identifier spellings are NOT covered
/// here — callers must separately compare
/// [`imports_clause_opaque_tokens`] (or fail closed on their presence).
pub fn imports_clause_idents(src: &str) -> Vec<String> {
    let tokens = tokenize(src);
    let mut out = Vec::new();
    let mut in_imports = false;
    for t in &tokens {
        if t.kind != TokenKind::Ident {
            continue;
        }
        let word = token_text(src, t);
        if !in_imports {
            if word == "imports" {
                in_imports = true;
            }
            continue;
        }
        if word == "begin" || word == "keywords" {
            break;
        }
        out.push(word.to_string());
    }
    out
}

// ===========================================================================
// imports_clause_opaque_tokens
// ===========================================================================

/// Source text of every token inside the `imports` clause (between the
/// `imports` keyword and its `begin`/`keywords` terminator) that is neither an
/// `Ident` nor a `Comment`: a quoted theory name (`imports "Tablet_X"` /
/// `imports "HOL-Probability.Probability"` — String, AltString, Cartouche,
/// Verbatim) or a stray symbol run. Such spellings are legal Isabelle and
/// elaborate fine in the probe, but they are OPAQUE to the `Ident`-only
/// clause scans ([`tablet_imports`] — the dep-cone walk feeding
/// `recursive_imports` and the payload cache key): a `Tablet`-shaped quoted
/// import carries a REAL dependency the cone key cannot see, so a dependent's
/// key would not change when the dependency does — a stale cert served.
/// Comments stay legal (they are masked payloads, never import names).
///
/// [`validate_imports`] runs its own clause scan (it distinguishes a
/// double-quoted ALLOWLISTED library theory — legal, since a hyphenated
/// session-qualified name has no bare spelling — from the `Tablet`-shaped /
/// non-String spellings it rejects). This collector stays kind-blind and
/// complete: it feeds the payload cache key (`runtime_cli_observations::
/// isabelle_cone_member_pinned_source` — refuse to construct a key over a
/// cone the walker cannot see when a token is `Tablet`-shaped, even for
/// content that never passed validation) and the proof-only delta skip's
/// order-insensitive multiset equality (the skip sorts both sides) comparison, both of which must see EVERY opaque token.
pub fn imports_clause_opaque_tokens(src: &str) -> Vec<String> {
    let tokens = tokenize(src);
    let mut out = Vec::new();
    let mut in_imports = false;
    for t in &tokens {
        if t.kind != TokenKind::Ident {
            if in_imports && t.kind != TokenKind::Comment {
                out.push(token_text(src, t).to_string());
            }
            continue;
        }
        let word = token_text(src, t);
        if !in_imports {
            if word == "imports" {
                in_imports = true;
            }
            continue;
        }
        if word == "begin" || word == "keywords" {
            break;
        }
    }
    out
}

// ===========================================================================
// validate_imports
// ===========================================================================

/// Validate the `imports` clause against the Isabelle import allowlist,
/// returning the offending import names (empty = valid). The Isabelle analogue
/// of `runtime_cli_observations::validate_imports` (the Lean `import Tablet.X` /
/// `Mathlib.*` allowlist). An import is legal iff it is:
///   * a `Tablet_<Dep>` intra-tablet theory (the sibling-node import, bare
///     spelling ONLY), OR
///   * exactly an allowed session-root prefix (`HOL`, `Main`), OR
///   * a dotted descendant of one (`HOL.List`, `HOL.Analysis.…`), OR
///   * a DOUBLE-QUOTED session-qualified library theory whose name passes the
///     same allowlist (`"HOL-Probability.Probability"`) — the ONLY legal
///     Isabelle spelling for a hyphenated session-qualified name (a hyphen is
///     a symbolic char, so the bare spelling does not parse). Lean parity:
///     the allowlist admits the `HOL-Analysis` / `HOL-Probability` roots, so
///     refusing the only legal spelling of them closed the door the allowlist
///     had deliberately opened.
///
/// A quoted import stays REJECTED when it names anything `Tablet`-shaped
/// (substring match, deliberately over-broad, in LOCKSTEP with
/// `runtime_cli_observations::isabelle_cone_member_pinned_source`'s
/// key-poisoning rule): the `tablet_imports` dep-cone walk is `Ident`-only, so
/// a quoted sibling import would hide a REAL dependency edge from
/// `recursive_imports` and the payload cache key — a stale cert served. The
/// lockstep guarantees every quoted import this validator ADMITS is outside
/// the cone-key's fail-closed set (it contributes no cone member; its content
/// identity is pinned by the cache key's toolchain-identity + `isabelle/ROOT`
/// lines). Alt-string / cartouche / verbatim / symbolic spellings have no
/// legitimate use in an imports clause and stay rejected outright.
///
/// Scans the `imports`-clause tokens specifically (between `imports` and the
/// `begin`/`keywords` terminator), so an out-of-allowlist name in a proof
/// body / comment / string is never misread as an import. The allowlist is
/// read from the IsabelleHol descriptor's `allowed_import_prefixes` (the
/// single source of truth).
pub fn validate_imports(src: &str) -> Vec<String> {
    let allowed = crate::backend::isabelle_hol_descriptor().allowed_import_prefixes;
    let tokens = tokenize(src);
    let mut violations = Vec::new();

    // Exactly a session root, or a dotted descendant of one.
    let allowlisted = |name: &str| -> bool {
        allowed
            .iter()
            .any(|prefix| name == *prefix || name.starts_with(&format!("{prefix}.")))
    };

    let mut in_imports = false;
    for t in &tokens {
        // Locate the `imports` keyword / the clause terminator on the Ident
        // channel; only tokens INSIDE the clause are judged.
        if t.kind == TokenKind::Ident {
            let word = token_text(src, t);
            if !in_imports {
                if word == "imports" {
                    in_imports = true;
                }
                continue;
            }
            if word == "begin" || word == "keywords" {
                break;
            }
            // Intra-tablet sibling import — always legal (the dep-graph edge).
            if word.starts_with("Tablet_") {
                continue;
            }
            // D4 — symmetry with the quoted channel. A bare import that merely
            // CONTAINS `Tablet` under an allowlisted root (`HOL.Tablet_X`) is
            // not a sibling spelling: `tablet_imports` records no cone member
            // for it, and the cone hash's opaque-token scan never sees it (it is
            // an Ident), so it would ride outside both dependency tracking and
            // the cache key. The quoted channel already rejects this shape.
            if word.contains("Tablet") {
                violations.push(format!(
                    "{word} (an import naming a Tablet theory must be the bare \
                     sibling spelling `Tablet_<Node>`, not a qualified or \
                     embedded form)"
                ));
                continue;
            }
            if !allowlisted(word) {
                violations.push(word.to_string());
            }
            continue;
        }
        if !in_imports || t.kind == TokenKind::Comment {
            // Outside the clause, or a masked comment payload — never an
            // import name.
            continue;
        }
        let offending = token_text(src, t);
        if t.kind != TokenKind::String {
            // Alt-string / cartouche / verbatim / symbolic spelling: legal
            // Isabelle but with no legitimate use as an import name. Reject.
            violations.push(format!(
                "{offending} (non-identifier import spelling is not allowed \
                 -- write the bare theory name, or a double-quoted \
                 session-qualified name like `imports \
                 \"HOL-Probability.Probability\"`)"
            ));
            continue;
        }
        // A double-quoted import. Unwrap the delimiters; anything that is not
        // a PLAIN session-qualified theory name (escapes, embedded quotes,
        // empty, non-name chars) is rejected as a spelling violation.
        let inner = offending
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or("");
        if !is_plain_quoted_theory_name(inner) {
            violations.push(format!(
                "{offending} (quoted import is not a plain theory name \
                 -- write a double-quoted session-qualified name like \
                 `imports \"HOL-Probability.Probability\"`)"
            ));
            continue;
        }
        // `Tablet`-shaped (over-broad substring, cone-key lockstep — see the
        // fn doc): a quoted sibling import hides a real dep edge from the
        // `Ident`-only cone walk. The bare spelling is the legal one.
        if inner.contains("Tablet") {
            violations.push(format!(
                "{offending} (quoted intra-tablet import is not allowed -- \
                 write the bare theory name instead, e.g. `imports Tablet_X`, \
                 not `imports \"Tablet_X\"`, so the dependency edge is \
                 visible to the import walker)"
            ));
            continue;
        }
        if !allowlisted(inner) {
            violations.push(format!(
                "{inner} (quoted import root is not in the Isabelle import \
                 allowlist)"
            ));
        }
    }

    violations
}

/// True iff `inner` (a double-quoted import's unwrapped payload) is a plain
/// session-qualified theory name: ASCII-letter-led components of letters /
/// digits / `_` / `'`, joined by `.` (theory qualification) or `-` (the
/// session-name hyphen, `HOL-Probability`). No escapes, no whitespace, no
/// leading/trailing/doubled separators — anything fancier is not a name a
/// worker legitimately imports and is rejected at the caller.
fn is_plain_quoted_theory_name(inner: &str) -> bool {
    if !inner.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    // D3 — every component must be LETTER-led, matching the Python derivation's
    // `_THEORY_NAME_RE`. Without this the worker gate accepts spellings
    // (`"HOL.9Foo"`, `"HOL._Foo"`, `"HOL.'a"`) that the session-preamble
    // derivation then rejects, so a preamble the kernel called valid silently
    // falls back to the historical constant. They name no resolvable theory
    // either, so nothing legitimate is lost.
    let mut prev_sep = true; // start-of-string behaves like just-after-separator
    for c in inner.chars() {
        if c == '.' || c == '-' {
            if prev_sep {
                return false;
            }
            prev_sep = true;
        } else if prev_sep {
            if !c.is_ascii_alphabetic() {
                return false;
            }
            prev_sep = false;
        } else if c.is_ascii_alphanumeric() || c == '_' || c == '\'' {
            prev_sep = false;
        } else {
            return false;
        }
    }
    !prev_sep
}

// ===========================================================================
// forbidden_keyword_line_hits
// ===========================================================================

/// The Isabelle analogue of `runtime_cli_observations::scan_forbidden_keywords`:
/// returns `(keyword, line, text)` for each forbidden command-keyword token in
/// the node theory, PLUS a synthetic `("sorry", line, …)` hit for the open
/// marker (so the caller's `forbidden_hits_include_textual_sorry` /
/// `sorry_free` derivation is correct for `.thy`). `line`/`text` are computed
/// from the token's byte offset. Uses the Isabelle tokenizer (comment / string
/// / cartouche masking is intrinsic), so a forbidden word inside a payload is
/// never a false hit — the same hazard-proofing `validate_node_shape` relies on.
///
/// The forbidden set is the descriptor's `forbidden_keywords` (the lexical
/// policy, including the S4 codegen methods) plus the cert-diagnostic commands
/// (`thm`/`thm_oracles`/`thm_deps`/`print_*`) `validate_node_shape` bans.
pub fn forbidden_keyword_line_hits(src: &str) -> Vec<(String, u32, String)> {
    let forbidden = crate::backend::ISABELLE_HOL_FORBIDDEN_KEYWORDS;
    let tokens = tokenize(src);
    let mut hits = Vec::new();
    let line_of = |byte: usize| -> u32 { (src[..byte].bytes().filter(|b| *b == b'\n').count() + 1) as u32 };
    let line_text = |byte: usize| -> String {
        let line_start = src[..byte].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = src[byte..].find('\n').map(|i| byte + i).unwrap_or(src.len());
        src[line_start..line_end].trim().to_string()
    };
    for t in &tokens {
        if !is_keyword_token(src, t) {
            continue;
        }
        let word = token_text(src, t);
        let is_forbidden = forbidden.contains(&word)
            || CERT_DIAGNOSTIC_COMMANDS.contains(&word)
            || word.starts_with("print_");
        if is_forbidden {
            hits.push((word.to_string(), line_of(t.start), line_text(t.start)));
        }
        if word == "sorry" {
            hits.push(("sorry".to_string(), line_of(t.start), line_text(t.start)));
        }
    }
    hits
}

// ===========================================================================
// shared
// ===========================================================================

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D3 — quoted-name components must be LETTER-led, matching the Python
    /// derivation's `_THEORY_NAME_RE`. Without lockstep the worker gate accepts
    /// spellings the session-preamble derivation rejects, so a preamble the
    /// kernel called valid silently falls back to the historical constant.
    #[test]
    fn quoted_theory_name_components_must_be_letter_led() {
        for bad in ["HOL.9Foo", "HOL._Foo", "HOL.'a", "HOL-9x.Foo"] {
            assert!(
                !is_plain_quoted_theory_name(bad),
                "{bad} must not pass the quoted-name shape gate"
            );
        }
        for good in ["HOL-Probability.Probability", "Main", "HOL.Nat", "HOL-Analysis.X_1"] {
            assert!(
                is_plain_quoted_theory_name(good),
                "{good} must pass the quoted-name shape gate"
            );
        }
    }

    /// D4 — the bare channel must reject `Tablet`-containing imports that are
    /// not the sibling spelling, matching the quoted channel. Such a name
    /// records no cone member and is invisible to the cone hash's opaque scan.
    #[test]
    fn bare_qualified_tablet_import_is_rejected_but_siblings_pass() {
        let qualified = "theory Tablet_Foo\n  imports HOL.Tablet_X\nbegin\n\nend\n";
        let errs = validate_imports(qualified);
        assert!(
            errs.iter().any(|e| e.contains("Tablet_X")),
            "a qualified Tablet import must be rejected: {errs:?}"
        );

        let sibling = "theory Tablet_Foo\n  imports Tablet_Bar Complex_Main\nbegin\n\nend\n";
        assert!(
            validate_imports(sibling).is_empty(),
            "a bare sibling import must stay legal: {:?}",
            validate_imports(sibling)
        );
    }


    /// Every surface that admits Isabelle/ML is rejected by the shape gate.
    ///
    /// The Isabelle accept-gate DROPS the axiom ceiling on the grounds that the
    /// node-level ban covers worker-authored axioms. Reachable ML reaches
    /// `Thm.add_axiom`, whose `PAxm` is a distinct proofterm constructor from
    /// `Oracle` and so never appears in `thm_oracles` — so a hole here is a hole
    /// in the live acceptance policy. `ML_prf` and the `tactic`/`raw_tactic`
    /// proof methods were all reachable: the scan matches tokens EXACTLY, and
    /// banning `ML` does not ban `ML_prf`.
    #[test]
    fn shape_gate_rejects_every_ml_entry_point() {
        for token in [
            "ML_prf", "ML_export", "ML_file_debug", "ML_file_no_debug",
            "SML_file", "SML_file_debug", "SML_file_no_debug",
            "SML_import", "SML_export", "simproc_setup",
            "tactic", "raw_tactic",
        ] {
            let src = format!(
                "theory Tablet_Foo\n  imports Tablet_Preamble\nbegin\n\n                 theorem Foo: \"True\"\n  {token}\n\nend\n"
            );
            let hits = forbidden_keyword_line_hits(&src);
            assert!(
                hits.iter().any(|(word, _, _)| word == token),
                "shape gate did not reject the ML entry point `{token}`"
            );
        }
    }


    const REPO: &str = "/dev/null";

    fn canonical_proof() -> &'static str {
        "\
theory Tablet_Foo
  imports Tablet_Preamble
begin

theorem Foo:
  fixes x :: nat
  shows \"x = x\"
proof -
  show \"x = x\" by simp
qed

end
"
    }

    // ---------------- split ----------------

    #[test]
    fn split_canonical_proof() {
        let src = canonical_proof();
        let s = split(src, "Foo").expect("split must succeed");
        assert_eq!(s.node, NodeId::from("Foo"));
        let stmt = &src[..s.body_marker_start_byte];
        let body = &src[s.body_marker_end_byte..];
        // Statement region includes the goal command + signature, NOT the proof.
        assert!(stmt.contains("theorem Foo:"));
        assert!(stmt.contains("shows"));
        assert!(!stmt.contains("by simp"), "proof must be outside the statement");
        // Body region starts at `proof`.
        assert!(body.trim_start().starts_with("proof"));
        assert!(body.contains("by simp"));
        // No-marker invariant: the two boundary bytes coincide.
        assert_eq!(s.body_marker_start_byte, s.body_marker_end_byte);
    }

    #[test]
    fn split_definition_node() {
        let src = "\
theory Tablet_Foo
  imports Main
begin
definition Foo :: \"nat \\<Rightarrow> nat\" where
  \"Foo x = x + 1\"
end
";
        let s = split(src, "Foo").expect("split must succeed");
        let stmt = &src[..s.body_marker_start_byte];
        assert!(stmt.contains("definition Foo"));
        // A definition has no Isar proof; the boundary is the next command
        // (`end`) — so the whole `definition …` span is the statement and the
        // body is just the trailing `end`.
        let body = &src[s.body_marker_end_byte..];
        assert!(body.contains("end"));
        assert!(stmt.contains("Foo x = x + 1"));
    }

    #[test]
    fn split_boundary_is_before_leading_using() {
        // `using`/`unfolding` are PROOF: the statement ends BEFORE them.
        let src = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"x = x\"
  using refl
  by blast
end
";
        let s = split(src, "Foo").expect("split");
        let stmt = &src[..s.body_marker_start_byte];
        let body = &src[s.body_marker_end_byte..];
        assert!(stmt.contains("lemma Foo:"));
        assert!(!stmt.contains("using"), "leading `using` is proof, not statement");
        assert!(body.trim_start().starts_with("using"));
    }

    // ---------------- split hazard cases (must NOT mis-split) ----------------

    #[test]
    fn split_hazard_proof_keyword_inside_string() {
        // A `proof`/`by`/`using` substring inside the inner-syntax string must
        // NOT be read as a proof-part token.
        let src = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"by using proof done = by using proof done\"
  by simp
end
";
        let s = split(src, "Foo").expect("split");
        let stmt = &src[..s.body_marker_start_byte];
        // The whole string is in the statement; the boundary is the REAL `by`.
        assert!(stmt.contains("by using proof done = by using proof done"));
        let body = &src[s.body_marker_end_byte..];
        assert!(body.trim_start().starts_with("by simp"));
    }

    #[test]
    fn split_hazard_proof_keyword_inside_cartouche() {
        // Unicode cartouche `‹…›` containing proof keywords.
        let src = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"P\"
  \u{2039}this proof by qed is a comment-like cartouche\u{203A}
  by simp
end
";
        let s = split(src, "Foo").expect("split");
        let body = &src[s.body_marker_end_byte..];
        // The boundary is the real `by simp`, not the cartouche interior. The
        // cartouche itself is a marginal annotation, after the statement, so it
        // sits in the body region (first proof-part token is the trailing `by`).
        assert!(body.contains("by simp"));
        // The statement does not absorb the trailing `by`.
        let stmt = &src[..s.body_marker_start_byte];
        assert!(stmt.contains("lemma Foo:"));
    }

    #[test]
    fn split_hazard_proof_keyword_inside_ascii_cartouche() {
        // ASCII `\<open>…\<close>` cartouche containing proof keywords.
        let src = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"P\" \\<open>by proof qed using\\<close>
  by simp
end
";
        let s = split(src, "Foo").expect("split");
        let body = &src[s.body_marker_end_byte..];
        assert!(body.contains("by simp"));
        let stmt = &src[..s.body_marker_start_byte];
        assert!(stmt.contains("lemma Foo:"));
        assert!(!stmt.contains("by simp"));
    }

    #[test]
    fn split_hazard_proof_keyword_inside_nested_comment() {
        // Nested `(* (* by proof *) using *)` must be fully skipped.
        let src = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"P\" (* outer (* by proof qed *) using done *)
  by simp
end
";
        let s = split(src, "Foo").expect("split");
        let body = &src[s.body_marker_end_byte..];
        assert!(body.contains("by simp"));
        let stmt = &src[..s.body_marker_start_byte];
        // The nested comment did not prematurely end the statement at a fake
        // `by`/`proof`.
        assert!(stmt.contains("lemma Foo:"));
        assert!(!stmt.contains("by simp"));
    }

    // ---------------- signature_hash ----------------

    #[test]
    fn signature_hash_stable_across_proof_body_edits() {
        let v1 = canonical_proof();
        let v2 = "\
theory Tablet_Foo
  imports Tablet_Preamble
begin

theorem Foo:
  fixes x :: nat
  shows \"x = x\"
  by (rule refl)

end
";
        let repo = Path::new(REPO);
        let h1 = signature_hash(repo, v1, "Foo").unwrap();
        let h2 = signature_hash(repo, v2, "Foo").unwrap();
        assert_eq!(h1, h2, "proof-body edit must not move the signature hash");
    }

    #[test]
    fn signature_hash_stable_across_imports_and_header_edits() {
        let v1 = canonical_proof();
        let v2 = "\
theory Tablet_Foo
  imports Tablet_Preamble Tablet_Helper HOL.Real
begin

theorem Foo:
  fixes x :: nat
  shows \"x = x\"
proof -
  show \"x = x\" by simp
qed

end
";
        let repo = Path::new(REPO);
        assert_eq!(
            signature_hash(repo, v1, "Foo").unwrap(),
            signature_hash(repo, v2, "Foo").unwrap(),
            "import/header edits must not move the signature hash"
        );
    }

    #[test]
    fn signature_hash_stable_across_symbol_spelling_rewrites() {
        // `∀`↔`\<forall>` and `‹›`↔`\<open>\<close>` rewrites in the SIGNATURE
        // must hash identically.
        let unicode = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"\u{2200}x. P x\" and \"Q \u{2039}note\u{203A}\"
  by auto
end
";
        let ascii = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"\\<forall>x. P x\" and \"Q \\<open>note\\<close>\"
  by auto
end
";
        let repo = Path::new(REPO);
        assert_eq!(
            signature_hash(repo, unicode, "Foo").unwrap(),
            signature_hash(repo, ascii, "Foo").unwrap(),
            "∀/\\<forall> and ‹›/\\<open>\\<close> must canonicalize to one hash"
        );
    }

    #[test]
    fn signature_hash_changes_on_statement_edit() {
        let v1 = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"P\"
  by simp
end
";
        let v2 = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"Q\"
  by simp
end
";
        let repo = Path::new(REPO);
        assert_ne!(
            signature_hash(repo, v1, "Foo").unwrap(),
            signature_hash(repo, v2, "Foo").unwrap(),
            "a statement edit (P→Q) MUST move the signature hash"
        );
    }

    // ---------------- declaration_kind ----------------

    #[test]
    fn declaration_kind_classifies_theory_goal_as_theorem_like() {
        // Every THEORY_GOAL keyword opens an Isar proof ⇒ "theorem_like".
        for goal_kw in ["theorem", "lemma", "corollary", "proposition"] {
            let src = format!(
                "theory Tablet_Foo\n  imports Main\nbegin\n{goal_kw} Foo: \"P\"\n  by simp\nend\n"
            );
            assert_eq!(
                declaration_kind(&src, "Foo"),
                "theorem_like",
                "`{goal_kw}` must classify as theorem_like",
            );
        }
    }

    #[test]
    fn declaration_kind_classifies_thy_decl_as_definition() {
        // `definition` / `fun` / `datatype` are non-proof-bearing ⇒ "definition".
        let def = "theory Tablet_Foo\n  imports Main\nbegin\ndefinition Foo :: \"nat \\<Rightarrow> nat\" where\n  \"Foo x = x + 1\"\nend\n";
        assert_eq!(declaration_kind(def, "Foo"), "definition");
        let fun_node = "theory Tablet_Foo\n  imports Main\nbegin\nfun Foo :: \"nat \\<Rightarrow> nat\" where\n  \"Foo 0 = 0\"\nend\n";
        assert_eq!(declaration_kind(fun_node, "Foo"), "definition");
        let datatype = "theory Tablet_Foo\n  imports Main\nbegin\ndatatype Foo = Bar | Baz\nend\n";
        assert_eq!(declaration_kind(datatype, "Foo"), "definition");
    }

    #[test]
    fn declaration_kind_empty_when_node_absent() {
        let src = "theory Tablet_Foo\n  imports Main\nbegin\ntheorem Other: \"P\"\n  by simp\nend\n";
        assert_eq!(
            declaration_kind(src, "Foo"),
            "",
            "no principal command named Foo ⇒ empty kind",
        );
    }

    // ---------------- validate_node_shape ----------------

    #[test]
    fn validate_accepts_canonical_proof_and_definition() {
        assert!(
            validate_node_shape(canonical_proof(), "Foo").is_empty(),
            "canonical proof node must validate: {:?}",
            validate_node_shape(canonical_proof(), "Foo")
        );
        let def = "\
theory Tablet_Foo
  imports Main
begin
definition Foo :: \"nat\" where \"Foo = 0\"
end
";
        assert!(
            validate_node_shape(def, "Foo").is_empty(),
            "definition node must validate: {:?}",
            validate_node_shape(def, "Foo")
        );
    }

    #[test]
    fn validate_rejects_keywords_clause() {
        let src = "\
theory Tablet_Foo
  imports Main
  keywords \"my_cmd\" :: thy_decl
begin
lemma Foo: \"P\" by simp
end
";
        let errs = validate_node_shape(src, "Foo");
        assert!(
            errs.iter().any(|e| e.contains("keywords")),
            "must reject a `keywords` clause: {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_ml_setup_oracle_axiomatization() {
        for (src_body, kw) in [
            ("ML \\<open>val x = 1\\<close>", "ML"),
            ("setup \\<open>Thm.declaration_attribute\\<close>", "setup"),
            ("oracle myor = \\<open>fn t => t\\<close>", "oracle"),
            ("axiomatization where bad: \"P\"", "axiomatization"),
        ] {
            let src = format!(
                "theory Tablet_Foo\n  imports Main\nbegin\nlemma Foo: \"P\" by simp\n{src_body}\nend\n"
            );
            let errs = validate_node_shape(&src, "Foo");
            assert!(
                errs.iter().any(|e| e.contains(kw)),
                "must reject forbidden command `{kw}`: {errs:?}"
            );
        }
    }

    #[test]
    fn validate_rejects_oops_with_specific_error() {
        let src = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"P\"
  oops
end
";
        let errs = validate_node_shape(src, "Foo");
        assert!(
            errs.iter()
                .any(|e| e.contains("oops") && e.to_ascii_lowercase().contains("abandons")),
            "oops must give a specific 'abandons the goal' shape error: {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_multiple_principal_commands() {
        let src = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"P\" by simp
lemma Bar: \"Q\" by simp
end
";
        let errs = validate_node_shape(src, "Foo");
        assert!(
            errs.iter().any(|e| e.contains("exactly one principal")),
            "must reject multiple principal commands: {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_name_mismatch() {
        let src = "\
theory Tablet_Foo
  imports Main
begin
lemma Bar: \"P\" by simp
end
";
        let errs = validate_node_shape(src, "Foo");
        assert!(
            errs.iter().any(|e| e.contains("must be named Foo")),
            "must reject a principal command not named Foo: {errs:?}"
        );
    }

    #[test]
    fn validate_does_not_reject_sorry() {
        // `sorry` is the OPEN marker, not a forbidden command — an open node
        // must still validate-shape-clean.
        let src = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"P\"
  sorry
end
";
        let errs = validate_node_shape(src, "Foo");
        assert!(
            errs.is_empty(),
            "an open (`sorry`) node must validate-shape-clean: {errs:?}"
        );
    }

    #[test]
    fn validate_does_not_trip_on_keyword_inside_string() {
        // A forbidden-command word inside an inner-syntax string is a payload,
        // never a command-keyword token, so it must not be rejected.
        let src = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"the word axiomatization and ML and oracle as text\"
  by simp
end
";
        let errs = validate_node_shape(src, "Foo");
        assert!(
            errs.is_empty(),
            "forbidden words inside a string must NOT trip the command scan: {errs:?}"
        );
    }

    // ------ tranche-2 shape tightening: post-proof / pre-declaration --------

    #[test]
    fn validate_rejects_notation_after_qed() {
        let src = "\
theory Tablet_Foo
  imports Main
begin
theorem Foo:
  shows \"x = x\"
proof -
  show \"x = x\" by simp
qed
notation Foo (\"FOO\")
end
";
        let errs = validate_node_shape(src, "Foo");
        assert!(
            errs.iter().any(|e| e.contains("`notation`")
                && e.contains("theory-context command")),
            "a post-qed `notation` must trip the theory-context ban: {errs:?}"
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("after the principal declaration's proof concludes")),
            "a post-qed command must trip the only-comments post-proof rule: {errs:?}"
        );
    }

    #[test]
    fn validate_accepts_comment_after_qed() {
        let src = "\
theory Tablet_Foo
  imports Main
begin
theorem Foo:
  shows \"x = x\"
proof -
  show \"x = x\" by simp
qed
(* verified against the paper's lines 12--14 *)
end
";
        let errs = validate_node_shape(src, "Foo");
        assert!(
            errs.is_empty(),
            "a comment between qed and end must stay legal: {errs:?}"
        );
    }

    /// Every operator-named theory-context command is rejected wherever it
    /// appears as a bare token: after the proof AND between `begin` and the
    /// principal declaration (the region the exactly-one-principal rule does
    /// not police — none of these is a principal command).
    #[test]
    fn validate_rejects_every_theory_context_command_in_both_regions() {
        for cmd in [
            "notation",
            "no_notation",
            "type_notation",
            "translations",
            "bundle",
            "unbundle",
            "interpretation",
            "locale",
            "hide_const",
            "hide_fact",
            "hide_type",
            "context",
        ] {
            let post_proof = format!(
                "theory Tablet_Foo\n  imports Main\nbegin\nlemma Foo: \"P\"\n  by simp\n{cmd} extra_arg\nend\n"
            );
            let errs = validate_node_shape(&post_proof, "Foo");
            assert!(
                errs.iter()
                    .any(|e| e.contains(&format!("`{cmd}`")) && e.contains("theory-context command")),
                "post-proof `{cmd}` must be rejected: {errs:?}"
            );
            let pre_decl = format!(
                "theory Tablet_Foo\n  imports Main\nbegin\n{cmd} extra_arg\nlemma Foo: \"P\"\n  by simp\nend\n"
            );
            let errs = validate_node_shape(&pre_decl, "Foo");
            assert!(
                errs.iter()
                    .any(|e| e.contains(&format!("`{cmd}`")) && e.contains("theory-context command")),
                "pre-declaration `{cmd}` must be rejected: {errs:?}"
            );
            assert!(
                errs.iter()
                    .any(|e| e.contains("between `begin` and the principal declaration")),
                "pre-declaration `{cmd}` must trip the region rule: {errs:?}"
            );
        }
    }

    /// Airtightness beyond the named list: an UNLISTED command after the
    /// proof (`named_theorems` is not in `THEORY_CONTEXT_COMMANDS`, not
    /// forbidden, not principal) is still rejected by the only-
    /// comments/whitespace post-proof rule.
    #[test]
    fn validate_rejects_unlisted_command_after_proof() {
        let src = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"P\"
  by simp
named_theorems foo_intros
end
";
        let errs = validate_node_shape(src, "Foo");
        assert!(
            errs.iter()
                .any(|e| e.contains("after the principal declaration's proof concludes")),
            "an unlisted post-proof command must trip the only-comments rule: {errs:?}"
        );
    }

    /// The same strong rule holds between `begin` and the principal
    /// declaration: only comments/whitespace, even for unlisted commands.
    #[test]
    fn validate_rejects_unlisted_command_before_declaration() {
        let src = "\
theory Tablet_Foo
  imports Main
begin
named_theorems foo_intros
lemma Foo: \"P\"
  by simp
end
";
        let errs = validate_node_shape(src, "Foo");
        assert!(
            errs.iter()
                .any(|e| e.contains("between `begin` and the principal declaration")),
            "an unlisted pre-declaration command must trip the region rule: {errs:?}"
        );
    }

    // ------ D1 audit fix: glued proof terminals (`ident..` / `ident.`) ------

    /// A proof terminal `.`/`..` glued to the preceding fact name
    /// (`frac_neg_frac..` — the idiomatic spelling in
    /// `HOL/Archimedean_Field.thy`) must lex as Ident + terminal Sym, never
    /// as one long Ident; interior dots keep long names one token.
    #[test]
    fn tokenize_splits_trailing_terminal_dots_from_long_idents() {
        let toks = tokenize("frac_neg_frac..");
        assert_eq!(toks.len(), 2, "toks={toks:?}");
        assert_eq!(toks[0].kind, TokenKind::Ident);
        assert_eq!(token_text("frac_neg_frac..", &toks[0]), "frac_neg_frac");
        assert_eq!(toks[1].kind, TokenKind::Sym);
        assert_eq!(token_text("frac_neg_frac..", &toks[1]), "..");

        let toks = tokenize("TrueI.");
        assert_eq!(toks.len(), 2, "toks={toks:?}");
        assert_eq!(token_text("TrueI.", &toks[0]), "TrueI");
        assert_eq!(toks[1].kind, TokenKind::Sym);
        assert_eq!(token_text("TrueI.", &toks[1]), ".");

        // Interior dots: long identifiers stay ONE token.
        for long in ["Thm.add_oracle", "HOL.List", "Foo.bar", "n1.n2.n3"] {
            let toks = tokenize(long);
            assert_eq!(toks.len(), 1, "`{long}` must stay one ident: {toks:?}");
            assert_eq!(toks[0].kind, TokenKind::Ident);
            assert_eq!(token_text(long, &toks[0]), long);
        }

        // Standalone terminals lex as the exact `.`/`..` Sym tokens the
        // keyword table classifies.
        let toks = tokenize(".. .");
        assert_eq!(toks.len(), 2);
        assert!(toks.iter().all(|t| t.kind == TokenKind::Sym));
        assert_eq!(token_text(".. .", &toks[0]), "..");
        assert_eq!(token_text(".. .", &toks[1]), ".");
    }

    /// SOUNDNESS (D1): a glued `..` terminal must not blind the goal-stack
    /// tracker — an unlisted theory-context command after the glued terminal
    /// is rejected by the post-proof only-comments rule.
    #[test]
    fn validate_rejects_command_after_glued_terminal_proof() {
        let src = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"frac (y - frac x) = frac (y - x)\"
    unfolding diff_conv_add_uminus frac_add frac_neg_frac..
global_interpretation foo_def
end
";
        let errs = validate_node_shape(src, "Foo");
        assert!(
            !errs.is_empty(),
            "a command smuggled after a glued terminal must be rejected"
        );
        assert!(
            errs.iter().any(|e| {
                e.contains("after the principal declaration's proof concludes")
                    && e.contains("`global_interpretation`")
            }),
            "the glued terminal must conclude the tracker so the post-proof \
             region rule names the smuggled command: {errs:?}"
        );
    }

    /// D1 control: a glued terminal with a CLEAN trailer (only the closing
    /// `end`) stays accepted — the tracker concludes at the glued `.`/`..`.
    #[test]
    fn validate_accepts_glued_terminal_proof_with_clean_trailer() {
        let glued_dotdot = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"frac (y - frac x) = frac (y - x)\"
    unfolding diff_conv_add_uminus frac_add frac_neg_frac..
end
";
        let errs = validate_node_shape(glued_dotdot, "Foo");
        assert!(errs.is_empty(), "glued `..` terminal must validate: {errs:?}");

        let glued_dot = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"True\"
  using TrueI.
end
";
        let errs = validate_node_shape(glued_dot, "Foo");
        assert!(errs.is_empty(), "glued `.` terminal must validate: {errs:?}");
    }

    /// SOUNDNESS (D1 backstop): a CLOSED node whose principal proof the
    /// goal-stack tracker cannot see conclude (`Ok(None)`) is rejected
    /// fail-closed; an OPEN node with the same untrackable tail keeps
    /// today's lenient behaviour.
    #[test]
    fn validate_rejects_closed_node_with_untrackable_proof_conclusion() {
        // Closed (no `sorry`): the structured proof never concludes (no
        // `qed`), so the post-proof region cannot be policed — reject.
        let closed = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"P\"
proof -
  show \"P\" by simp
end
";
        let errs = validate_node_shape(closed, "Foo");
        assert!(
            errs.iter()
                .any(|e| e.contains("does not conclude trackably")),
            "a closed node with an unconcluded principal proof must fail \
             closed: {errs:?}"
        );

        // Open (`sorry` closes only the nested goal): same unconcluded
        // shape, but the node is open — keep today's behaviour (clean).
        let open = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"P\"
proof -
  have \"Q\" sorry
end
";
        let errs = validate_node_shape(open, "Foo");
        assert!(
            errs.is_empty(),
            "an open node keeps the lenient unconcluded-tail behaviour: {errs:?}"
        );
    }

    /// D4: a bare KNOWN COMMAND after the concluding `qed`/`by` must not be
    /// swallowed as a fake trailing method — the only-comments post-proof
    /// rule rejects it. Real trailing methods (`qed auto`) stay accepted
    /// (pinned by `validate_accepts_trailing_methods_on_concluding_terminal`).
    #[test]
    fn validate_rejects_bare_command_as_fake_trailing_method() {
        for cmd in ["unused_thms", "print_theorems", "thm"] {
            let src = format!(
                "theory Tablet_Foo\n  imports Main\nbegin\nlemma Foo: \"P\"\nproof -\n  show \"P\" by simp\nqed {cmd}\nend\n"
            );
            let errs = validate_node_shape(&src, "Foo");
            assert!(
                errs.iter()
                    .any(|e| e.contains("after the principal declaration's proof concludes")),
                "`qed {cmd}` must leave the command in the post-proof region \
                 (not swallow it as a method): {errs:?}"
            );
        }
        // A PRINCIPAL command in method position is not swallowed either;
        // it surfaces via the exactly-one-principal rule instead.
        let src = "theory Tablet_Foo\n  imports Main\nbegin\nlemma Foo: \"P\"\nproof -\n  show \"P\" by simp\nqed lemmas\nend\n";
        let errs = validate_node_shape(src, "Foo");
        assert!(
            errs.iter().any(|e| e.contains("exactly one principal")),
            "`qed lemmas` must be rejected (principal-command rule): {errs:?}"
        );
    }

    /// Trailing method arguments of the concluding `by`/`qed` belong to the
    /// proof, not to the post-proof region.
    #[test]
    fn validate_accepts_trailing_methods_on_concluding_terminal() {
        let by_two_methods = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"P\"
  by (cases x) (auto simp: y_def)+
end
";
        let errs = validate_node_shape(by_two_methods, "Foo");
        assert!(
            errs.is_empty(),
            "a two-method terminal `by` must validate clean: {errs:?}"
        );
        let qed_method = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"P\"
proof (cases y)
qed (auto)
end
";
        let errs = validate_node_shape(qed_method, "Foo");
        assert!(
            errs.is_empty(),
            "a `qed <method>` conclusion must validate clean: {errs:?}"
        );
    }

    /// `subgoal`/apply-script proofs conclude at their `done`, not at the
    /// nested subgoal terminals — no false post-proof violation.
    #[test]
    fn validate_accepts_subgoal_apply_script_shapes() {
        let src = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"P \\<and> Q\"
  apply (rule conjI)
  subgoal by simp
  subgoal by simp
  done
end
";
        let errs = validate_node_shape(src, "Foo");
        assert!(
            errs.is_empty(),
            "subgoal apply-script proofs must validate clean: {errs:?}"
        );
    }

    /// 2–3 REAL accepted shapes from the live tablet (verbatim copies:
    /// a definition node, an open `sorry` theorem with a pre-declaration
    /// comment, and the deeply nested structured proof of the paper's main
    /// theorem) keep validating clean under the tightened gate.
    #[test]
    fn validate_accepts_real_accepted_tablet_shapes() {
        let component_def = r#"theory Tablet_Component
  imports Tablet_Reachable
begin

definition Component :: "nat \<Rightarrow> (nat \<times> nat \<Rightarrow> bool) \<Rightarrow> nat \<Rightarrow> nat set" where
  "Component n omega u = {v \<in> VertexSet n. Reachable n omega u v}"

end
"#;
        let errs = validate_node_shape(component_def, "Component");
        assert!(errs.is_empty(), "live definition node must validate: {errs:?}");

        let open_theorem = r#"theory Tablet_ConnectedNoIsolated
  imports Tablet_GraphConnected Tablet_Isolated Tablet_Reachable Tablet_Adjacent
begin

(* Paper lines 476--480: connectivity excludes isolated vertices for n >= 2. *)
theorem ConnectedNoIsolated:
  fixes n :: nat and omega :: "nat \<times> nat \<Rightarrow> bool"
  assumes "2 \<le> n"
    and "GraphConnected n omega"
  shows "\<forall>v \<in> VertexSet n. \<not> Isolated n omega v"
  sorry

end
"#;
        let errs = validate_node_shape(open_theorem, "ConnectedNoIsolated");
        assert!(errs.is_empty(), "live open node must validate: {errs:?}");

        let nested_closed = r#"theory Tablet_ConnectivityThreshold
  imports Tablet_SubcriticalConnectivity Tablet_CriticalConnectivity
    Tablet_SupercriticalConnectivity
begin

(* Paper Theorem 1, lines 63--70: the three connectivity-threshold regimes. *)
theorem ConnectivityThreshold:
  fixes p :: "nat \<Rightarrow> real"
  assumes "\<And>n. 0 \<le> p n"
    and "\<And>n. p n \<le> 1"
  shows "(filterlim (ThresholdOffset p) at_bot at_top \<longrightarrow>
      filterlim (\<lambda>n. enn2real (ConnectionProbability n (p n)))
        (nhds 0) at_top) \<and>
    (\<forall>c :: real. filterlim (ThresholdOffset p) (nhds c) at_top \<longrightarrow>
      filterlim (\<lambda>n. enn2real (ConnectionProbability n (p n)))
        (nhds (exp (- exp (- c)))) at_top) \<and>
    (filterlim (ThresholdOffset p) at_top at_top \<longrightarrow>
      filterlim (\<lambda>n. enn2real (ConnectionProbability n (p n)))
        (nhds 1) at_top)"
proof (rule conjI)
  show "filterlim (ThresholdOffset p) at_bot at_top \<longrightarrow>
      filterlim (\<lambda>n. enn2real (ConnectionProbability n (p n)))
        (nhds 0) at_top"
  proof
    assume h_subcritical: "filterlim (ThresholdOffset p) at_bot at_top"
    show "filterlim (\<lambda>n. enn2real (ConnectionProbability n (p n)))
      (nhds 0) at_top"
      by (rule SubcriticalConnectivity[OF assms(1) assms(2) h_subcritical])
  qed
next
  show "(\<forall>c :: real. filterlim (ThresholdOffset p) (nhds c) at_top \<longrightarrow>
      filterlim (\<lambda>n. enn2real (ConnectionProbability n (p n)))
        (nhds (exp (- exp (- c)))) at_top) \<and>
    (filterlim (ThresholdOffset p) at_top at_top \<longrightarrow>
      filterlim (\<lambda>n. enn2real (ConnectionProbability n (p n)))
        (nhds 1) at_top)"
  proof (rule conjI)
    show "\<forall>c :: real. filterlim (ThresholdOffset p) (nhds c) at_top \<longrightarrow>
        filterlim (\<lambda>n. enn2real (ConnectionProbability n (p n)))
          (nhds (exp (- exp (- c)))) at_top"
    proof
      fix c :: real
      show "filterlim (ThresholdOffset p) (nhds c) at_top \<longrightarrow>
        filterlim (\<lambda>n. enn2real (ConnectionProbability n (p n)))
          (nhds (exp (- exp (- c)))) at_top"
      proof
        assume h_critical: "filterlim (ThresholdOffset p) (nhds c) at_top"
        show "filterlim (\<lambda>n. enn2real (ConnectionProbability n (p n)))
          (nhds (exp (- exp (- c)))) at_top"
          by (rule CriticalConnectivity[OF assms(1) assms(2) h_critical])
      qed
    qed
  next
    show "filterlim (ThresholdOffset p) at_top at_top \<longrightarrow>
        filterlim (\<lambda>n. enn2real (ConnectionProbability n (p n)))
          (nhds 1) at_top"
    proof
      assume h_supercritical: "filterlim (ThresholdOffset p) at_top at_top"
      show "filterlim (\<lambda>n. enn2real (ConnectionProbability n (p n)))
        (nhds 1) at_top"
        by (rule SupercriticalConnectivity[OF assms(1) assms(2) h_supercritical])
    qed
  qed
qed

end
"#;
        let errs = validate_node_shape(nested_closed, "ConnectivityThreshold");
        assert!(
            errs.is_empty(),
            "live nested structured proof must validate: {errs:?}"
        );
    }

    #[test]
    fn imports_clause_idents_collects_full_list() {
        let src = "theory Tablet_Foo\n  imports Tablet_Dep Main \"HOL-Library.Foo\"\nbegin\nlemma Foo: \"P\" by simp\nend\n";
        assert_eq!(
            imports_clause_idents(src),
            vec!["Tablet_Dep".to_string(), "Main".to_string()],
            "every bare import ident — library roots included — must be collected"
        );
        assert_eq!(
            imports_clause_opaque_tokens(src),
            vec!["\"HOL-Library.Foo\"".to_string()],
            "the quoted spelling stays on the opaque channel"
        );
    }

    // ---------- B2c-gate Slice 3 — S4 codegen-method / __Cert / cert-cmd bans -------

    /// `apply (eval)` / `by normalization` / `by code_simp` — the codegen-
    /// trusting proof METHODS (S4) — must be rejected, with the codegen-method
    /// diagnostic (R3: the scan reaches method positions because every Ident is
    /// a keyword-candidate).
    #[test]
    fn validate_rejects_codegen_methods_with_method_message() {
        for (proof, kw) in [
            ("apply (eval)", "eval"),
            ("by normalization", "normalization"),
            ("by code_simp", "code_simp"),
        ] {
            let src = format!(
                "theory Tablet_Foo\n  imports Main\nbegin\nlemma Foo: \"P = P\"\n  {proof}\nend\n"
            );
            let errs = validate_node_shape(&src, "Foo");
            assert!(
                errs.iter()
                    .any(|e| e.contains(kw) && e.to_ascii_lowercase().contains("code generator")),
                "codegen method `{kw}` must be rejected with the code-generator message: {errs:?}"
            );
        }
    }

    /// A node whose registered name ends `__Cert` collides with the server's
    /// probe-theory path `Tablet_<Node>__Cert.thy`; reject it.
    #[test]
    fn validate_rejects_cert_suffixed_node_name() {
        let src = "\
theory Tablet_Bad__Cert
  imports Main
begin
lemma Bad__Cert: \"P\" by simp
end
";
        let errs = validate_node_shape(src, "Bad__Cert");
        assert!(
            errs.iter()
                .any(|e| e.contains("__Cert") && e.to_ascii_lowercase().contains("reserved")),
            "a `__Cert`-suffixed node name must be rejected as reserved: {errs:?}"
        );
    }

    /// The cert-diagnostic commands (`thm`/`thm_oracles`/`thm_deps`/`print_*`)
    /// the SERVER injects must be rejected in a NODE theory.
    #[test]
    fn validate_rejects_cert_diagnostic_commands() {
        for (cmd, body) in [
            ("thm_oracles", "thm_oracles Foo"),
            ("thm_deps", "thm_deps Foo"),
            ("thm", "thm Foo"),
            ("print_theorems", "print_theorems"),
            ("print_statement", "print_statement Foo"),
        ] {
            let src = format!(
                "theory Tablet_Foo\n  imports Main\nbegin\nlemma Foo: \"P\" by simp\n{body}\nend\n"
            );
            let errs = validate_node_shape(&src, "Foo");
            assert!(
                errs.iter().any(|e| e.contains(cmd) && e.to_ascii_lowercase().contains("diagnostic")),
                "cert-diagnostic command `{cmd}` must be rejected: {errs:?}"
            );
        }
    }

    /// An `ML ‹…›` node must be rejected (already in the forbidden set, but pin
    /// the cartouche-arg variant).
    #[test]
    fn validate_rejects_ml_cartouche_node() {
        let src = "\
theory Tablet_Foo
  imports Main
begin
ML \\<open>writeln \"hi\"\\<close>
lemma Foo: \"P\" by simp
end
";
        let errs = validate_node_shape(src, "Foo");
        assert!(
            errs.iter().any(|e| e.contains("ML")),
            "an `ML ‹…›` node must be rejected: {errs:?}"
        );
    }

    // ---------------- validate_imports ----------------

    /// A legit `imports HOL.List` + a sibling `Tablet_Preamble` validate clean.
    #[test]
    fn validate_imports_accepts_hol_descendant_and_tablet_sibling() {
        let src = "\
theory Tablet_Foo
  imports Main HOL.List Tablet_Preamble
begin
lemma Foo: \"P\" by simp
end
";
        assert!(
            validate_imports(src).is_empty(),
            "HOL.List / Main / Tablet_Preamble must all be allowed: {:?}",
            validate_imports(src)
        );
    }

    /// An out-of-allowlist `imports Some_AFP` must be flagged.
    #[test]
    fn validate_imports_rejects_out_of_allowlist() {
        let src = "\
theory Tablet_Foo
  imports Main Some_AFP
begin
lemma Foo: \"P\" by simp
end
";
        let violations = validate_imports(src);
        assert!(
            violations.iter().any(|v| v == "Some_AFP"),
            "out-of-allowlist import `Some_AFP` must be flagged: {violations:?}"
        );
    }

    /// A forbidden word inside the proof body / a string is NOT an import, so
    /// `validate_imports` (which scans only the `imports` clause) ignores it.
    #[test]
    fn validate_imports_ignores_non_import_tokens() {
        let src = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"Some_AFP is just a word here\" by simp
end
";
        assert!(
            validate_imports(src).is_empty(),
            "a non-import occurrence of an out-of-allowlist name must NOT be flagged: {:?}",
            validate_imports(src)
        );
    }

    /// A QUOTED theory name in the imports clause is judged, not blanket-
    /// banned: a `Tablet`-shaped quoted import stays REJECTED (it would hide
    /// a real dep-cone edge from the `Ident`-only clause scans — the stale-
    /// cert hazard, unchanged), and an out-of-allowlist quoted import is
    /// rejected HONESTLY as not-allowlisted (previously it "failed" only as a
    /// spelling violation). The bare sibling spelling stays legal.
    #[test]
    fn validate_imports_rejects_quoted_import_token() {
        // The stale-cert shape: a quoted SIBLING import the cone walk misses.
        let quoted_sibling = "\
theory Tablet_Foo
  imports Tablet_Preamble \"Tablet_X\"
begin
lemma Foo: \"P\" by simp
end
";
        let violations = validate_imports(quoted_sibling);
        assert!(
            violations
                .iter()
                .any(|v| v.contains("\"Tablet_X\"") && v.contains("bare theory name")),
            "a quoted intra-tablet import must be rejected, naming the \
             offending text and directing the worker to the bare spelling: \
             {violations:?}"
        );

        // The smuggling shape: an out-of-allowlist session import. Now
        // rejected for the honest reason — the allowlist — not the spelling.
        let smuggled = "\
theory Tablet_Foo
  imports Main \"HOL-Library.Foo\"
begin
lemma Foo: \"P\" by simp
end
";
        let violations = validate_imports(smuggled);
        assert!(
            violations
                .iter()
                .any(|v| v.contains("HOL-Library.Foo") && v.contains("allowlist")),
            "a quoted out-of-allowlist import must be flagged as \
             not-allowlisted: {violations:?}"
        );

        // Control: the bare sibling spelling stays legal.
        let bare = "\
theory Tablet_Foo
  imports Tablet_Preamble Tablet_X
begin
lemma Foo: \"P\" by simp
end
";
        assert!(
            validate_imports(bare).is_empty(),
            "bare `imports Tablet_X` must stay legal: {:?}",
            validate_imports(bare)
        );
    }

    /// A double-quoted ALLOWLISTED session-qualified import is legal — the
    /// only legal Isabelle spelling of a hyphenated name (`HOL-Probability`
    /// is on the allowlist, and `HOL-Probability.Probability` has no bare
    /// spelling: the hyphen is a symbolic char). This is the Lean-parity fix:
    /// the allowlist deliberately admits the `HOL-Analysis`/`HOL-Probability`
    /// roots, so refusing their only legal spelling closed that door.
    #[test]
    fn validate_imports_accepts_quoted_allowlisted_import() {
        // The live wedge shape: a PMF node importing the probability library.
        let node = "\
theory Tablet_Foo
  imports Tablet_Preamble \"HOL-Probability.Probability\"
begin
lemma Foo: \"P\" by simp
end
";
        assert!(
            validate_imports(node).is_empty(),
            "a quoted allowlisted session-qualified import must be legal: {:?}",
            validate_imports(node)
        );

        // The preamble shape (the import root's own clause).
        let preamble = "\
theory Tablet_Preamble
  imports \"HOL-Probability.Probability\"
begin
end
";
        assert!(
            validate_imports(preamble).is_empty(),
            "the worker preamble's quoted allowlisted import must be legal: {:?}",
            validate_imports(preamble)
        );

        // An allowlisted ROOT spelled quoted, and an HOL-Analysis descendant.
        let more = "\
theory Tablet_Foo
  imports \"Main\" \"HOL-Analysis.Convex\" Complex_Main
begin
lemma Foo: \"P\" by simp
end
";
        assert!(
            validate_imports(more).is_empty(),
            "quoted `Main` / `HOL-Analysis.Convex` must be legal: {:?}",
            validate_imports(more)
        );
    }

    /// The cone-key LOCKSTEP: any quoted import whose text contains `Tablet`
    /// (over-broad substring, matching `isabelle_cone_member_pinned_source`'s
    /// key-poisoning rule) is rejected even when its prefix would pass the
    /// allowlist — so validate_imports never admits a token that poisons
    /// cache-key construction.
    #[test]
    fn validate_imports_rejects_tablet_shaped_quoted_import_even_under_allowed_prefix() {
        let src = "\
theory Tablet_Foo
  imports Main \"HOL.Tablet_Smuggle\"
begin
lemma Foo: \"P\" by simp
end
";
        let violations = validate_imports(src);
        assert!(
            violations
                .iter()
                .any(|v| v.contains("HOL.Tablet_Smuggle") && v.contains("bare theory name")),
            "a Tablet-shaped quoted import must be rejected regardless of its \
             prefix: {violations:?}"
        );
    }

    /// Non-String opaque spellings (alt-string / cartouche) and malformed
    /// quoted payloads (escapes) have no legitimate use as import names and
    /// stay rejected.
    #[test]
    fn validate_imports_rejects_non_string_and_malformed_quoted_spellings() {
        // Alt-string spelling.
        let alt = "\
theory Tablet_Foo
  imports `Main`
begin
lemma Foo: \"P\" by simp
end
";
        let violations = validate_imports(alt);
        assert!(
            violations
                .iter()
                .any(|v| v.contains("`Main`") && v.contains("non-identifier")),
            "an alt-string import spelling must be rejected: {violations:?}"
        );

        // Cartouche spelling.
        let cart = "\
theory Tablet_Foo
  imports \u{2039}Main\u{203A}
begin
lemma Foo: \"P\" by simp
end
";
        let violations = validate_imports(cart);
        assert!(
            violations.iter().any(|v| v.contains("non-identifier")),
            "a cartouche import spelling must be rejected: {violations:?}"
        );

        // A quoted payload with an escape is not a plain theory name.
        let escaped = "\
theory Tablet_Foo
  imports \"HOL\\\".List\"
begin
lemma Foo: \"P\" by simp
end
";
        let violations = validate_imports(escaped);
        assert!(
            !violations.is_empty(),
            "an escaped quoted import must be rejected: {violations:?}"
        );

        // Doubled / trailing separators are not plain theory names.
        for bad in ["\"HOL..List\"", "\"HOL-\"", "\"-Analysis\"", "\"\""] {
            let src = format!(
                "theory Tablet_Foo\n  imports {bad}\nbegin\nlemma Foo: \"P\" by simp\nend\n"
            );
            assert!(
                !validate_imports(&src).is_empty(),
                "malformed quoted import {bad} must be rejected"
            );
        }
    }

    /// `is_plain_quoted_theory_name` unit pins (the quoted-import shape gate).
    #[test]
    fn plain_quoted_theory_name_shape() {
        for good in [
            "Main",
            "HOL-Probability.Probability",
            "HOL-Analysis.Convex",
            "HOL.List",
            "Foo'",
        ] {
            assert!(is_plain_quoted_theory_name(good), "{good} must be plain");
        }
        for bad in ["", "1Foo", "HOL..List", "HOL-", "-HOL", "HOL List", "a\\b"] {
            assert!(!is_plain_quoted_theory_name(bad), "{bad:?} must not be plain");
        }
    }

    /// A comment inside the imports clause stays legal: the quoted-import
    /// rejection targets opaque NAME spellings, not masked payloads.
    #[test]
    fn validate_imports_allows_comment_in_imports_clause() {
        let src = "\
theory Tablet_Foo
  imports Main (* the base session *) Tablet_Preamble
begin
lemma Foo: \"P\" by simp
end
";
        assert!(
            validate_imports(src).is_empty(),
            "a comment in the imports clause must stay legal: {:?}",
            validate_imports(src)
        );
    }

    // ---------------- forbidden_keyword_line_hits ----------------

    /// The line-hit scanner surfaces the open `sorry` marker and a forbidden
    /// command, with line numbers, and masks string payloads.
    #[test]
    fn forbidden_keyword_line_hits_surfaces_sorry_and_forbidden() {
        let src = "\
theory Tablet_Foo
  imports Main
begin
lemma Foo: \"P\"
  sorry
ML \\<open>x\\<close>
end
";
        let hits = forbidden_keyword_line_hits(src);
        assert!(
            hits.iter().any(|(k, _, _)| k == "sorry"),
            "sorry must surface as a hit: {hits:?}"
        );
        assert!(
            hits.iter().any(|(k, _, _)| k == "ML"),
            "ML must surface as a forbidden hit: {hits:?}"
        );
        // A clean node yields no hits.
        let clean = canonical_proof();
        assert!(
            forbidden_keyword_line_hits(clean).is_empty(),
            "a clean node yields no forbidden/sorry hits: {:?}",
            forbidden_keyword_line_hits(clean)
        );
    }

    // ---------------- is_node_open ----------------

    #[test]
    fn is_node_open_true_on_sorry() {
        let src = "theory Tablet_Foo\n  imports Main\nbegin\nlemma Foo: \"P\"\n  sorry\nend\n";
        assert!(is_node_open(src));
    }

    #[test]
    fn is_node_open_true_on_raw_proof_placeholder() {
        let src = "theory Tablet_Foo\n  imports Main\nbegin\nlemma Foo: \"P\" \\<proof>\nend\n";
        assert!(is_node_open(src));
    }

    #[test]
    fn is_node_open_false_on_masked_sorry() {
        // `sorry` inside a string / comment / cartouche ⇒ NOT open.
        let in_string =
            "theory Tablet_Foo\n  imports Main\nbegin\nlemma Foo: \"sorry is just a word\"\n  by simp\nend\n";
        assert!(!is_node_open(in_string), "sorry in a string is not open");
        let in_comment =
            "theory Tablet_Foo\n  imports Main\nbegin\nlemma Foo: \"P\" (* TODO sorry later *)\n  by simp\nend\n";
        assert!(!is_node_open(in_comment), "sorry in a comment is not open");
        let in_cartouche = "theory Tablet_Foo\n  imports Main\nbegin\nlemma Foo: \"P\" \u{2039}sorry\u{203A}\n  by simp\nend\n";
        assert!(!is_node_open(in_cartouche), "sorry in a cartouche is not open");
    }

    #[test]
    fn is_node_open_false_on_qed_and_oops() {
        let qed = canonical_proof();
        assert!(!is_node_open(qed), "a closed qed proof is not open");
        // `oops` is a shape error, NOT "open".
        let oops = "theory Tablet_Foo\n  imports Main\nbegin\nlemma Foo: \"P\"\n  oops\nend\n";
        assert!(!is_node_open(oops), "oops is not 'open' (it abandons the goal)");
    }

    // ---------------- tablet_imports ----------------

    #[test]
    fn tablet_imports_extracts_tablet_deps() {
        let src = "\
theory Tablet_Foo
  imports Tablet_Preamble Tablet_Dep HOL.Real Main
begin
lemma Foo: \"P\" by simp
end
";
        let deps = tablet_imports(src);
        assert_eq!(
            deps,
            vec![NodeId::from("Preamble"), NodeId::from("Dep")],
            "only Tablet_-prefixed imports become node deps; HOL.Real/Main are skipped"
        );
    }

    #[test]
    fn tablet_imports_empty_when_no_tablet_deps() {
        let src = "theory Tablet_Foo\n  imports Main HOL.Real\nbegin\nlemma Foo: \"P\" by simp\nend\n";
        assert!(tablet_imports(src).is_empty());
    }

    // ---------------- tokenizer direct ----------------

    #[test]
    fn tokenize_nested_comment_is_one_opaque_token() {
        let src = "a (* x (* y *) z *) b";
        let toks = tokenize(src);
        // a, comment, b.
        assert_eq!(toks.len(), 3);
        assert_eq!(toks[0].kind, TokenKind::Ident);
        assert_eq!(toks[1].kind, TokenKind::Comment);
        assert_eq!(token_text(src, &toks[1]), "(* x (* y *) z *)");
        assert_eq!(toks[2].kind, TokenKind::Ident);
        assert_eq!(token_text(src, &toks[2]), "b");
    }

    #[test]
    fn tokenize_string_escape_and_symbol_token() {
        let src = "shows \"a \\\" b\" \\<forall>";
        let toks = tokenize(src);
        assert_eq!(toks[0].kind, TokenKind::Ident); // shows
        assert_eq!(toks[1].kind, TokenKind::String);
        assert_eq!(token_text(src, &toks[1]), "\"a \\\" b\"");
        // `\<forall>` is a single Sym token (not a cartouche, not a keyword).
        assert_eq!(toks[2].kind, TokenKind::Sym);
        assert_eq!(token_text(src, &toks[2]), "\\<forall>");
    }

    #[test]
    fn tokenize_ascii_cartouche_nests_with_unicode() {
        // Mixed-spelling nested cartouche: `\<open> ‹ \<close> ›` is balanced.
        let src = "\\<open>a \u{2039}b\u{203A} c\\<close>";
        let toks = tokenize(src);
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, TokenKind::Cartouche);
        assert_eq!(token_text(src, &toks[0]), src);
    }

    #[test]
    fn tokenize_terminates_on_unicode_letters_in_outer_position() {
        // Regression: the Ident arm was gated on Unicode `is_alphanumeric()` but
        // advanced only on ASCII `is_ident_char()`, so a Unicode letter/numeral
        // in outer (non-payload) position made a zero-width `Ident` and spun
        // forever. `λ` is in this module's own symbol table and is ubiquitous in
        // HOL terms — every public fn calls `tokenize` first, so all would hang.
        for src in [
            "\u{3bb}",                                  // λ alone
            "\u{3bb}x. x",                              // a λ-term in outer position
            "\u{3b1}",                                  // α (Greek letter)
            "\u{4e2d}",                                 // 中 (CJK)
            "\u{0663}",                                 // ٣ (Arabic-Indic digit)
            "theorem foo: \"\u{3bb}x. x = x\" by simp", // λ in a real statement
        ] {
            let toks = tokenize(src);
            assert!(!toks.is_empty(), "tokenize({src:?}) produced no tokens");
            assert!(
                toks.iter().all(|t| t.end > t.start),
                "tokenize({src:?}) produced a zero-width token (the hang signature): {toks:?}"
            );
        }
        // A bare `λ` in outer position is one `Sym` token (handled like `\<forall>`).
        let toks = tokenize("\u{3bb}");
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, TokenKind::Sym);
    }
}

