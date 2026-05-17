//! #126 Tier-3 — RA-native-downrank for safe proc-macro-off (DEFAULT-OFF).
//!
//! The biggest single RAM lever: RA proc-macro-off is ≈ **−53 % RSS**
//! (#74 measurement). The *safe half already ships* — #74's
//! `detect_proc_macro` auto-turns it off for projects with **no**
//! proc-macro dependency. The unsafe half is forcing it off on a
//! project that **does** use proc-macros (Leptos `view!`, serde
//! derive): with RA proc-macro-off, RA cannot expand those macros and
//! emits **false** `severity:Error` ("unresolved", type errors) with
//! `source:"rust-analyzer"` for every macro-generated item. The
//! F8-redo rule (#55: *any-source* severity:Error ⇒ per-file RED) then
//! folds that hallucination into a **persistent false-RED on every
//! proc-macro project** — fail-safe (never false-GREEN) but unusable.
//!
//! ## The fix (validation-pass)
//!
//! When this flag is set, the authoritative per-file verdict is driven
//! by the **`source:"rustc"` (cargo-check) tier ONLY** —
//! [`PublishDiagnostics::has_authoritative_error`] instead of the
//! F8-redo any-source [`has_any_severity_error`]. RA-native
//! `severity:Error` is *demoted to advisory* (still in the `native`
//! map + diagnostics list + the #21 advisory channel — visible, just
//! not verdict-driving). cargo-check expands proc-macros **itself**
//! (it is a real `cargo check` subprocess, wholly independent of RA's
//! `procMacro.enable`), so it remains the **complete** authority.
//!
//! ## No-wrong-verdict invariant (load-bearing — D-PROCMACRO-DOWNRANK §4)
//!
//! *No false-GREEN* — GREEN still requires `flycheck_done` + zero
//! `source:"rustc"` errors; cargo-check compiled WITH proc-macro
//! expansion, so any real error ⇒ rustc error ⇒ not GREEN.
//! *No false-RED* — RA-native hallucinations are `source:"rust-
//! analyzer"`, excluded from the rustc-only RED set.
//! *No missed real RED* — every real error is caught by the complete
//! cargo-check authority. The *only* loss is the F8-redo latency
//! accelerator: a genuine syntax error surfaces at cargo-check-
//! completion instead of instantly via RA-native — a latency
//! regression, never a correctness one; eventual verdict colour is
//! identical and `never-publish-red` is untouched.
//!
//! ## Default-off
//!
//! Enabled iff `TF_RA_PROCMACRO_OFF=1`. Unset ⇒ [`enabled`] is
//! `false`: `InitOpts` keeps #74 auto-detect, the model keeps the
//! F8-redo any-source rule — **byte-identical to pre-#126**. Operator
//! doctrine: ship behind the flag, measure RSS delta, data decides
//! v0-default-vs-v0.1.

/// True iff `TF_RA_PROCMACRO_OFF=1` (strict; idiom matches
/// `TF_STRUCTURAL_TRIGGER` / `TF_RA_IDLE_EVICT`). Any other value ⇒
/// default-off ⇒ pre-#126 behavior, byte-identical.
pub fn enabled() -> bool {
    matches!(std::env::var("TF_RA_PROCMACRO_OFF").as_deref(), Ok("1"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn enabled_is_strict_one_default_off() {
        // `enabled()` reads process env (unsafe to mutate across
        // threads on edition 2024); pin the parse RULE via a mirror —
        // same discipline as structural::enabled / idle::enabled.
        fn rule(v: Option<&str>) -> bool {
            v == Some("1")
        }
        assert!(rule(Some("1")));
        assert!(!rule(None));
        assert!(!rule(Some("")));
        assert!(!rule(Some("0")));
        assert!(!rule(Some("true")));
        assert!(!rule(Some("off")));
    }
}
