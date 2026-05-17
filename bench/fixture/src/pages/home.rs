//! Landing page: a counter wired to a metrics panel so a single signal flows
//! through several `view!`-heavy components.

use leptos::*;

use crate::components::counter::Counter;
use crate::components::metrics::MetricsPanel;

#[component]
pub fn HomePage() -> impl IntoView {
    let (progress, set_progress) = create_signal(0i32);

    let bump = move |_| set_progress.update(|p| *p += 7);
    let drain = move |_| set_progress.update(|p| *p = (*p - 3).max(0));

    view! {
        <div class="page page-home">
            <header class="page-head">
                <h2>"Inner loop"</h2>
                <p>
                    "The codebase always knows what works, and tells you the "
                    "moment it doesn't."
                </p>
            </header>

            <div class="page-cols">
                <Counter/>
                <MetricsPanel count=progress target=42/>
            </div>

            <div class="page-actions">
                <button on:click=bump>"+7"</button>
                <button on:click=drain>"-3"</button>
            </div>
        </div>
    }
}
