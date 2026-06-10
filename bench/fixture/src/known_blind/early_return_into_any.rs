//! KNOWN-BLIND corpus file (#A8) — DO NOT add to the module tree.
//!
//! Class: **early-return `view!` branch not unified with the tail
//! branch**. Each `view!` root expands to a distinct concrete
//! `HtmlElement<_>` type; a function returning `impl IntoView` needs ONE
//! concrete type, so the early `return` and the tail expression must be
//! unified with `.into_any()` on both. Without it, `cargo build` fails
//! (E0308 mismatched types, visible only AFTER Leptos proc-macro
//! expansion) while the daemon's RA-native verdict stays green — the
//! tf-mv #4070 incident class. See README.md in this directory.

use leptos::*;

#[component]
pub fn LoadGate(ready: ReadSignal<bool>, label: String) -> impl IntoView {
    if !ready.get_untracked() {
        // Missing `.into_any()` here (and on the tail below) is the bug:
        // this branch is HtmlElement<Span>, the tail is
        // HtmlElement<Article>.
        return view! { <span class="loading">"loading…"</span> };
    }
    view! { <article class="ready">{label}</article> }
}
