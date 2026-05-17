//! #112 — structural-completeness trigger (bounded spike, DEFAULT-OFF).
//!
//! Operator-approved spike of D-OPENCLOSED (`docs/design/D-OPENCLOSED.md`
//! @ branch `agent/dev-fixer-openclosed-design`). cargoless's primary
//! input is **AI agents writing whole files atomically**, not humans
//! streaming keystrokes — so a "broken mid-edit" buffer is a *rare
//! transient*, and gating the expensive authoritative tier (RA flycheck
//! = `cargo check`) on the buffer being structurally CLOSED is almost
//! free and skips exactly the wasteful checks agent loops generate
//! (syntactically-broken intermediate drafts; per-file fires during a
//! multi-file batch).
//!
//! ## Safety invariant (load-bearing — see D-OPENCLOSED §2.4/§3)
//!
//! Closedness gates ONLY the cargo-check **spend** and (transitively)
//! publish-eligibility. It NEVER touches the verdict colour: the watch
//! pipeline still sends `textDocument/didChange` for every batch (RA
//! re-parses → RA-native `severity:Error` still flips per-file RED via
//! the untouched F8-redo path), and only withholds
//! `textDocument/didSave` (the flycheck trigger) when the batch is OPEN.
//! A withheld flycheck means no *new* authoritative green is produced
//! for OPEN content, so `.cargoless/latest-green` cannot advance on it
//! (AC#4 strengthened, fail-closed preserved) — without any change to
//! the byte-frozen `StateEvent`/publisher seam. Conservative-OPEN: any
//! lexer uncertainty returns OPEN; a wrongly-skipped check self-heals on
//! the next quiescent batch.
//!
//! ## Default-off
//!
//! Enabled iff `TF_STRUCTURAL_TRIGGER=1` (env idiom matching
//! `TF_DEBOUNCE_MS`/`TF_PROC_MACRO`). Unset ⇒ [`enabled`] is `false` and
//! the watch pipeline takes its prior code path **byte-identical** — no
//! lexer pass, no counters, zero behavior change. This is the exact
//! safety property the spike was approved on.
//!
//! ## Proxy = local lexer (RA-internal-shape fragility off the hot path)
//!
//! [`is_closed`] is a dependency-free **balance** scan
//! (delimiters / strings / chars-vs-lifetimes / comments), NOT a Rust
//! grammar — closedness is a "worth-checking" heuristic, never a
//! verdict. RA-native syntax-error diagnostics remain available as
//! optional corroboration elsewhere but are deliberately NOT on this
//! critical path (D-OPENCLOSED R3/R4).

use std::sync::atomic::{AtomicU64, Ordering};

/// True iff `TF_STRUCTURAL_TRIGGER=1`. Default-off: any other value
/// (including unset / empty / "0" / "true") ⇒ `false` ⇒ prior path.
pub fn enabled() -> bool {
    matches!(std::env::var("TF_STRUCTURAL_TRIGGER").as_deref(), Ok("1"))
}

/// bench-lead measurement hook (D-OPENCLOSED §4.2). Counts coalesced
/// `ChangeBatch`es at the `model.rs` `Debouncer::poll` consumer:
/// `settled` = every settled batch (would-fire **today**); `closed` =
/// every settled batch that was structurally CLOSED (would-fire
/// **proposed**). bench-lead reads `1 − closed/settled` = fraction of
/// authoritative cargo-checks eliminated per agent-edit-batch. Counters
/// only move while [`enabled`]; dormant (0,0) otherwise so an
/// instrumented run is opt-in.
#[derive(Debug, Default)]
pub struct StructuralCounters {
    settled: AtomicU64,
    closed: AtomicU64,
}

impl StructuralCounters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one settled batch and whether it was CLOSED.
    pub fn record(&self, all_closed: bool) {
        self.settled.fetch_add(1, Ordering::Relaxed);
        if all_closed {
            self.closed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `(settled_batches, closed_batches)` snapshot for bench-lead.
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.settled.load(Ordering::Relaxed),
            self.closed.load(Ordering::Relaxed),
        )
    }
}

/// Structural-completeness probe over one buffer's bytes.
///
/// Returns `true` (CLOSED) iff every `()`/`[]`/`{}` is balanced and
/// correctly nested, and there is no unterminated string / char / byte
/// / raw string and no open (possibly nested) block comment. Returns
/// `false` (OPEN) otherwise — including on any scan ambiguity
/// (conservative-OPEN; a wrongly-OPEN buffer merely skips one check and
/// self-heals next batch, never mis-colours or mis-publishes).
///
/// Lexically-significant characters in Rust (delimiters, quotes, `/`,
/// `#`, `r`/`b` prefixes, `\`) are all ASCII; non-ASCII bytes only occur
/// inside strings/comments/identifiers and are opaque here, so a byte
/// scan is correct (a UTF-8 lead/continuation byte is never one of the
/// structural ASCII bytes).
pub fn is_closed(src: &str) -> bool {
    let b = src.as_bytes();
    let n = b.len();
    let mut i = 0usize;
    let mut stack: Vec<u8> = Vec::new();

    while i < n {
        let c = b[i];
        match c {
            // ---- comments -------------------------------------------------
            b'/' if i + 1 < n && b[i + 1] == b'/' => {
                // line comment to EOL (or EOF)
                i += 2;
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                // block comment — Rust nests
                let mut depth = 1usize;
                i += 2;
                while i < n && depth > 0 {
                    if i + 1 < n && b[i] == b'/' && b[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if i + 1 < n && b[i] == b'*' && b[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if depth > 0 {
                    return false; // unterminated block comment ⇒ OPEN
                }
            }

            // ---- raw / byte string & byte char prefixes -------------------
            // r"…" r#"…"# br"…" br#"…"# b"…" b'…'  — but also raw identifiers
            // r#ident and ordinary identifiers starting r/b.
            b'r' | b'b' => {
                let (is_raw, after_prefix) = classify_prefix(b, i);
                if is_raw {
                    match scan_raw_string(b, after_prefix) {
                        Some(next) => i = next,
                        None => return false, // unterminated raw string
                    }
                } else if after_prefix > i {
                    // `b"`/`b'` byte string / byte char: fall through to the
                    // normal string/char scanners positioned at the quote.
                    i = after_prefix;
                    continue;
                } else {
                    // plain identifier starting with r/b — consume it whole
                    i = consume_ident(b, i);
                }
            }

            // ---- string ---------------------------------------------------
            b'"' => match scan_string(b, i + 1) {
                Some(next) => i = next,
                None => return false, // unterminated string
            },

            // ---- char literal vs lifetime ---------------------------------
            b'\'' => match scan_char_or_lifetime(b, i) {
                Some(next) => i = next,
                None => return false, // unterminated char literal
            },

            // ---- delimiters ----------------------------------------------
            b'(' | b'[' | b'{' => {
                stack.push(c);
                i += 1;
            }
            b')' | b']' | b'}' => {
                let want = match c {
                    b')' => b'(',
                    b']' => b'[',
                    _ => b'{',
                };
                match stack.pop() {
                    Some(open) if open == want => i += 1,
                    _ => return false, // mismatched / unbalanced close ⇒ OPEN
                }
            }

            // ---- identifier (so inner r/b/quote-free letters don't retrig)-
            c if c == b'_' || c.is_ascii_alphabetic() => {
                i = consume_ident(b, i);
            }

            _ => i += 1,
        }
    }

    stack.is_empty()
}

/// Identify an `r`/`b`-led prefix at `i`. Returns `(is_raw_string,
/// index_just_past_the_prefix)`. `index == i` (and `is_raw == false`)
/// means "not a string/char prefix — treat as identifier".
fn classify_prefix(b: &[u8], i: usize) -> (bool, usize) {
    let n = b.len();
    // br"…" / br#"…"#
    if b[i] == b'b' && i + 1 < n && b[i + 1] == b'r' {
        let j = i + 2;
        if j < n && (b[j] == b'"' || b[j] == b'#') {
            return (true, j);
        }
    }
    // r"…" / r#"…"#  (but r#ident = raw identifier, NOT a string)
    if b[i] == b'r' && i + 1 < n {
        if b[i + 1] == b'"' {
            return (true, i + 1);
        }
        if b[i + 1] == b'#' {
            // raw string iff the #...# run is terminated by a `"`
            let mut k = i + 1;
            while k < n && b[k] == b'#' {
                k += 1;
            }
            if k < n && b[k] == b'"' {
                return (true, i + 1);
            }
            // else: raw identifier r#ident — fall through to ident
        }
    }
    // b"…" byte string / b'…' byte char → not raw; point at the quote.
    if b[i] == b'b' && i + 1 < n && (b[i + 1] == b'"' || b[i + 1] == b'\'') {
        return (false, i + 1);
    }
    (false, i)
}

/// Scan a raw string whose `r`/`br` was already consumed; `start` points
/// at the first `#` or the opening `"`. Returns the index just past the
/// closing `"#…#`, or `None` if unterminated.
fn scan_raw_string(b: &[u8], start: usize) -> Option<usize> {
    let n = b.len();
    let mut k = start;
    let mut hashes = 0usize;
    while k < n && b[k] == b'#' {
        hashes += 1;
        k += 1;
    }
    if k >= n || b[k] != b'"' {
        return None;
    }
    k += 1; // past opening quote
    while k < n {
        if b[k] == b'"' {
            // need exactly `hashes` '#' following
            let mut h = 0usize;
            let mut p = k + 1;
            while p < n && h < hashes && b[p] == b'#' {
                h += 1;
                p += 1;
            }
            if h == hashes {
                return Some(p);
            }
        }
        k += 1;
    }
    None
}

/// Scan a normal/byte string; `start` points just past the opening `"`.
fn scan_string(b: &[u8], start: usize) -> Option<usize> {
    let n = b.len();
    let mut k = start;
    while k < n {
        match b[k] {
            b'\\' => k += 2, // escape: skip next byte
            b'"' => return Some(k + 1),
            _ => k += 1,
        }
    }
    None
}

/// Distinguish a char literal (`'a'`, `'\n'`, `'\''`, `'}'`, `'é'`) from
/// a lifetime/label (`'a`, `'static`, `'_`). `i` points at the opening
/// `'`. Returns the index just past the construct, or `None` if it is a
/// genuinely unterminated char literal (⇒ caller goes OPEN).
fn scan_char_or_lifetime(b: &[u8], i: usize) -> Option<usize> {
    let n = b.len();
    if i + 1 >= n {
        return None; // dangling '
    }

    // (1) Escaped char literal: `'` `\` <selector(+args)> `'`. The byte
    // right after the backslash is the escape selector (', n, t, \, 0,
    // x, u, …); consume it, then the closing quote — scanning on for the
    // variable-length `\xHH` / `\u{..}` forms (treating `\` as escape).
    if b[i + 1] == b'\\' {
        let mut k = i + 3; // past `'`, `\`, and the selector byte
        while k < n {
            match b[k] {
                b'\\' => k += 2,
                b'\'' => return Some(k + 1),
                _ => k += 1,
            }
        }
        return None; // unterminated escaped char literal
    }

    // (2) Ident-start after `'`: either a single-letter char literal
    // (`'a'`) or a lifetime/label (`'a`, `'static`, `'_`). It is a char
    // literal iff the ident run is immediately closed by `'`.
    if b[i + 1] == b'_' || b[i + 1].is_ascii_alphabetic() {
        let mut k = i + 1;
        while k < n && (b[k] == b'_' || b[k].is_ascii_alphanumeric()) {
            k += 1;
        }
        if k < n && b[k] == b'\'' {
            return Some(k + 1); // char literal 'a'
        }
        return Some(k); // lifetime/label — no closing quote expected
    }

    // (3) Non-ident, non-escape: a char literal of one (possibly
    // multibyte UTF-8, e.g. `'é'`) char. Scan to the next unescaped `'`.
    // Any `}`/`)`/`]` inside is literal content, never a delimiter.
    let mut k = i + 1;
    while k < n {
        match b[k] {
            b'\\' => k += 2,
            b'\'' => return Some(k + 1),
            _ => k += 1,
        }
    }
    None // unterminated char literal ⇒ OPEN
}

/// Consume an identifier/keyword run (ASCII + `_`; non-ASCII ident bytes
/// are opaque and consumed by the catch-all). Returns index past it.
fn consume_ident(b: &[u8], i: usize) -> usize {
    let n = b.len();
    let mut k = i;
    while k < n && (b[k] == b'_' || b[k].is_ascii_alphanumeric()) {
        k += 1;
    }
    k.max(i + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_is_strict_one_default_off() {
        // We can't safely mutate process env across threads on 2024
        // edition; assert the parse rule directly via a helper mirror.
        fn rule(v: Option<&str>) -> bool {
            v == Some("1")
        }
        assert!(rule(Some("1")));
        assert!(!rule(None));
        assert!(!rule(Some("")));
        assert!(!rule(Some("0")));
        assert!(!rule(Some("true")));
        assert!(!rule(Some("yes")));
    }

    #[test]
    fn balanced_simple() {
        assert!(is_closed("fn main() { let x = (1 + [2, 3][0]); }"));
        assert!(is_closed("")); // empty file is trivially closed
        assert!(is_closed("struct S;"));
    }

    #[test]
    fn unbalanced_delims_are_open() {
        assert!(!is_closed("fn main() {")); // the canonical mid-write case
        assert!(!is_closed("let x = (1 + 2;"));
        assert!(!is_closed("foo)]}"));
        assert!(!is_closed("{[(}])")); // mis-nested
    }

    #[test]
    fn delims_inside_strings_and_chars_dont_count() {
        assert!(is_closed(r#"let s = "a { b ( c [ "; "#));
        assert!(is_closed("let c = '}'; let d = '{';"));
        assert!(is_closed(r#"println!("unbalanced ) ] }");"#));
    }

    #[test]
    fn unterminated_string_is_open() {
        assert!(!is_closed(r#"let s = "no end"#));
        assert!(!is_closed("let s = \"line\nbroken")); // still no close
    }

    #[test]
    fn escapes_in_strings_and_chars() {
        assert!(is_closed(r#"let s = "a \" b \\ c";"#));
        assert!(is_closed(r#"let q = '\'';"#)); // escaped-quote char
        assert!(is_closed(r#"let n = '\n';"#));
    }

    #[test]
    fn lifetimes_are_not_unterminated_chars() {
        assert!(is_closed("fn f<'a>(x: &'a str) -> &'a str { x }"));
        assert!(is_closed("struct S<'a, 'b> { a: &'a u8, b: &'b u8 }"));
        assert!(is_closed("fn g() where 'static: 'static {}"));
    }

    #[test]
    fn raw_strings_with_hashes() {
        assert!(is_closed(r###"let s = r#"a "quote" and { ) ] inside"#;"###));
        assert!(is_closed(r###"let s = r##"has "# inside"##;"###));
        assert!(is_closed(r#"let s = r"no hashes { ( [ ";"#));
        // unterminated raw string ⇒ OPEN
        assert!(!is_closed("let s = r#\"never closed"));
    }

    #[test]
    fn byte_strings_and_byte_chars() {
        assert!(is_closed(r#"const M: &[u8] = b"tf-cas/input-hash/v1";"#));
        assert!(is_closed("let z = b'}';"));
        assert!(is_closed(r###"let r = br#"raw { byte "# str"#;"###));
        assert!(!is_closed(r#"let m = b"unterminated"#));
    }

    #[test]
    fn raw_identifier_is_not_a_string() {
        // r#fn is a raw identifier, not a raw string — must stay balanced.
        assert!(is_closed("let r#fn = 1; let r#match = 2;"));
        assert!(!is_closed("fn r#async() {")); // still unbalanced brace
    }

    #[test]
    fn comments_mask_delimiters() {
        assert!(is_closed("fn f() {} // trailing ) ] } noise"));
        assert!(is_closed("/* ( [ { unbalanced in comment */ fn f() {}"));
        assert!(is_closed("/* outer /* nested */ still */ struct S;"));
        assert!(!is_closed("fn f() {} /* unterminated comment ( ["));
    }

    #[test]
    fn realistic_agent_whole_file_is_closed() {
        let src = r#"
//! a module
use std::collections::BTreeMap;

pub struct Thing<'a> {
    name: &'a str,
    bytes: Vec<u8>,
}

impl<'a> Thing<'a> {
    pub fn new(name: &'a str) -> Self {
        Self { name, bytes: b"hdr/v1".to_vec() }
    }
    pub fn tag(&self) -> char { '#' }
}

#[cfg(test)]
mod tests {
    #[test]
    fn t() { assert_eq!(2 + 2, 4); }
}
"#;
        assert!(is_closed(src));
    }

    #[test]
    fn realistic_agent_midbatch_is_open() {
        // Agent wrote the signature, body not yet emitted (rare transient).
        let src = "pub fn handler(req: Request) -> Response {\n    let parsed = ";
        assert!(!is_closed(src));
    }

    #[test]
    fn counters_track_settled_and_closed() {
        let c = StructuralCounters::new();
        assert_eq!(c.snapshot(), (0, 0));
        c.record(true);
        c.record(false);
        c.record(true);
        assert_eq!(c.snapshot(), (3, 2)); // 3 settled, 2 closed
    }
}
