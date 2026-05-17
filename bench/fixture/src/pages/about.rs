//! About page: static-ish content with a small derived table so it is not
//! entirely trivial `view!`.

use leptos::*;

use crate::util::format::thousands;

struct Fact {
    label: &'static str,
    value: i64,
}

#[component]
pub fn AboutPage() -> impl IntoView {
    let facts = vec![
        Fact { label: "components", value: 5 },
        Fact { label: "pages", value: 3 },
        Fact { label: "domain types", value: 7 },
        Fact { label: "approx lines", value: 1100 },
    ];

    // Pre-collect the rows OUTSIDE the macro (no `<For>`, no turbofish in
    // attribute position — leptos 0.6.15 rsx rejects both).
    let rows = move || {
        facts
            .iter()
            .map(|f| {
                let (l, v) = (f.label, f.value);
                view! { <tr><td>{l}</td><td>{thousands(v)}</td></tr> }
            })
            .collect::<Vec<_>>()
    };

    view! {
        <div class="page page-about">
            <header class="page-head">
                <h2>"About"</h2>
                <p>
                    "This is the cargoless reference fixture used by the S1 "
                    "spike and AC#2 latency harness."
                </p>
            </header>
            <table class="about-table">
                <thead>
                    <tr><th>"metric"</th><th>"value"</th></tr>
                </thead>
                <tbody>{rows}</tbody>
            </table>
        </div>
    }
}
