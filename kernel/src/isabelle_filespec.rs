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
    /// keyword-kind table classifies. (Isabelle's `short_ident`/`long_ident`;
    /// we keep dots so `Thm.add_oracle` is one token.)
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

/// True iff `c` may appear in an outer alphanumeric identifier word. Isabelle
/// idents allow letters, digits, `_`, `'`, and `.` (long idents); we keep `.`
/// so a qualified command name like `Thm.add_oracle` tokenizes as one word.
fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '\'' || c == '.'
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
            while j < len {
                let cj = src[j..].chars().next().unwrap();
                if is_ident_char(cj) {
                    j += cj.len_utf8();
                } else {
                    break;
                }
            }
            tokens.push(Token { kind: TokenKind::Ident, start: i, end: j });
            i = j;
            continue;
        }

        // Otherwise: a run of symbolic characters (operators, punctuation,
        // braces). Stop at whitespace, ident chars, or any delimiter opener so
        // those are handled by their own arms next iteration. Braces `{`/`}`
        // and the proof terminals `.`/`..` are Sym runs but we cut them as
        // single-char tokens so the keyword table can see `{`/`}` exactly.
        if ch == '{' || ch == '}' {
            tokens.push(Token { kind: TokenKind::Sym, start: i, end: i + 1 });
            i += 1;
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
    const CERT_DIAGNOSTIC_COMMANDS: &[&str] = &["thm_oracles", "thm_deps", "thm"];
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
// validate_imports
// ===========================================================================

/// Validate the `imports` clause against the Isabelle import allowlist,
/// returning the offending import names (empty = valid). The Isabelle analogue
/// of `runtime_cli_observations::validate_imports` (the Lean `import Tablet.X` /
/// `Mathlib.*` allowlist). An import is legal iff it is:
///   * a `Tablet_<Dep>` intra-tablet theory (the sibling-node import), OR
///   * exactly an allowed session-root prefix (`HOL`, `Main`), OR
///   * a dotted descendant of one (`HOL.List`, `HOL.Analysis.…`).
///
/// Scans the `imports`-clause `Ident` tokens specifically (between `imports`
/// and the `begin`/`keywords` terminator), so an out-of-allowlist name in a
/// proof body / comment / string is never misread as an import. The allowlist
/// is read from the IsabelleHol descriptor's `allowed_import_prefixes` (the
/// single source of truth).
pub fn validate_imports(src: &str) -> Vec<String> {
    let allowed = crate::backend::isabelle_hol_descriptor().allowed_import_prefixes;
    let tokens = tokenize(src);
    let mut violations = Vec::new();

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
        // Intra-tablet sibling import — always legal (the dep-graph edge).
        if word.starts_with("Tablet_") {
            continue;
        }
        // Exactly a session root, or a dotted descendant of one.
        let ok = allowed.iter().any(|prefix| {
            word == *prefix || word.starts_with(&format!("{prefix}."))
        });
        if !ok {
            violations.push(word.to_string());
        }
    }

    violations
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
    const CERT_DIAGNOSTIC_COMMANDS: &[&str] = &["thm_oracles", "thm_deps", "thm"];
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
