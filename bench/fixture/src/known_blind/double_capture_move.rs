//! KNOWN-BLIND corpus file (#A8) — DO NOT add to the module tree.
//!
//! Class: **the same non-`Copy` variable captured twice inside one
//! `view!` without an intermediate `let`**. The macro expansion moves
//! `desc` into the attribute position and then uses it again for the
//! child text node: `cargo build` fails (E0382 use of moved value,
//! visible only AFTER Leptos proc-macro expansion) while the daemon's
//! RA-native verdict stays green — the tf-mv #4070 incident class. The
//! fix is `let title = desc.clone();` BEFORE the `view!`. See README.md
//! in this directory.

use leptos::*;

#[component]
pub fn SummaryCard(desc: String) -> impl IntoView {
    view! {
        <section class="summary" title=desc.clone()>
            <p>{desc}</p>
        </section>
    }
}
