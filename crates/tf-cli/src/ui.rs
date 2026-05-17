//! Terminal output: clear, non-noisy, actionable. One line per state
//! change, a fixed prefix vocabulary, colour only on a real TTY (never when
//! piped, so headless logs stay greppable). std-only.

use std::io::{IsTerminal, Write};

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

    fn color(self) -> &'static str {
        match self {
            Tag::Step => "36",
            Tag::Ok => "32",
            Tag::Wait => "90",
            Tag::Warn => "33",
            Tag::Error => "31",
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
    let _ = err.write_all(line.as_bytes());
}

pub fn step(msg: impl AsRef<str>) {
    emit(Tag::Step, msg.as_ref());
}

pub fn ok(msg: impl AsRef<str>) {
    emit(Tag::Ok, msg.as_ref());
}

pub fn wait(msg: impl AsRef<str>) {
    emit(Tag::Wait, msg.as_ref());
}

pub fn warn(msg: impl AsRef<str>) {
    emit(Tag::Warn, msg.as_ref());
}

pub fn error(msg: impl AsRef<str>) {
    emit(Tag::Error, msg.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_two_col_and_distinct() {
        let all = [Tag::Step, Tag::Ok, Tag::Wait, Tag::Warn, Tag::Error];
        for t in all {
            assert_eq!(t.glyph().len(), 2);
            assert!(!t.color().is_empty());
        }
        let s: std::collections::BTreeSet<_> = all.iter().map(|t| t.glyph()).collect();
        assert_eq!(s.len(), all.len());
    }

    #[test]
    fn emit_never_panics_without_tty() {
        step("s");
        ok("o");
        wait("w");
        warn("!");
        error("x");
    }
}
