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
                <tbody>
                    <For
                        each=move || {
                            facts
                                .iter()
                                .map(|f| (f.label, f.value))
                                .collect::<Vec<_>>()
                        }
                        key=|(l, _)| *l
                        let:row
                    >
                        <tr>
                            <td>{row.0}</td>
                            <td>{thousands(row.1)}</td>
                        </tr>
                    </For>
                </tbody>
            </table>
        </div>
    }
}
