//! Terminal output: clear, non-noisy, actionable.
//!
//! The product promise is "tells you the moment it doesn't" — so output is
//! *information-dense and quiet*, not a scrolling log. One line per state
//! change, a stable prefix vocabulary, colour only when stderr is a real
//! terminal (never when piped/redirected, so logs stay greppable). std-only:
//! no `owo-colors`/`tracing` — the vocabulary is six prefixes.

use std::io::{IsTerminal, Write};

/// Severity → fixed prefix. The set is deliberately tiny so scanning output
/// is muscle-memory: `>>` step, `ok` good, `..` waiting, `!!` warning,
/// `xx` error.
#[derive(Clone, Copy)]
enum Tag {
    Step,
    Ok,
    Wait,
    Warn,
    Error,
}

impl Tag {
    fn glyph(self) -> &'static str {
        match self {
            Tag::Step => ">>",
            Tag::Ok => "ok",
            Tag::Wait => "..",
            Tag::Warn => "!!",
            Tag::Error => "xx",
        }
    }

    /// ANSI colour code, used only on a TTY.
    fn color(self) -> &'static str {
        match self {
            Tag::Step => "36",  // cyan
            Tag::Ok => "32",    // green
            Tag::Wait => "90",  // bright black
            Tag::Warn => "33",  // yellow
            Tag::Error => "31", // red
        }
    }
}

fn emit(tag: Tag, msg: &str) {
    let mut err = std::io::stderr();
    let line = if err.is_terminal() {
        format!("\x1b[{}m{}\x1b[0m {msg}\n", tag.color(), tag.glyph())
    } else {
        format!("{} {msg}\n", tag.glyph())
    };
    // Best-effort: a closed stderr must never crash the daemon.
    let _ = err.write_all(line.as_bytes());
}

/// A discrete action is starting (e.g. "watching src/", "binding :8080").
pub fn step(msg: impl AsRef<str>) {
    emit(Tag::Step, msg.as_ref());
}

/// Something is now good / ready.
pub fn ok(msg: impl AsRef<str>) {
    emit(Tag::Ok, msg.as_ref());
}

/// Waiting on a slow but expected thing (cold build, RA indexing).
pub fn wait(msg: impl AsRef<str>) {
    emit(Tag::Wait, msg.as_ref());
}

/// Non-fatal: degraded but proceeding (RA missing, no WASM signal).
pub fn warn(msg: impl AsRef<str>) {
    emit(Tag::Warn, msg.as_ref());
}

/// Fatal-for-this-command. The message must already be actionable.
pub fn error(msg: impl AsRef<str>) {
    emit(Tag::Error, msg.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_are_two_columns_and_distinct() {
        let all = [Tag::Step, Tag::Ok, Tag::Wait, Tag::Warn, Tag::Error];
        for t in all {
            assert_eq!(t.glyph().len(), 2, "prefixes align in a 2-col gutter");
            assert!(!t.color().is_empty());
        }
        // Distinct glyphs so output is unambiguous when colour is stripped.
        let glyphs: std::collections::BTreeSet<_> = all.iter().map(|t| t.glyph()).collect();
        assert_eq!(glyphs.len(), all.len());
    }

    #[test]
    fn emit_never_panics_without_a_tty() {
        // Exercises the non-terminal formatting branch in CI (stderr piped).
        step("starting");
        ok("ready");
        wait("cold build");
        warn("degraded");
        error("actionable failure");
    }
}
