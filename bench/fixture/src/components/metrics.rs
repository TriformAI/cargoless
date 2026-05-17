//! Metrics panel. This component is the benchmark's **`view!` macro-error
//! target**: the harness injects an error *inside* the `view!` body (a method
//! that does not exist on the signal) to measure whether — and how fast —
//! rust-analyzer surfaces a diagnostic that only exists *after* Leptos
//! proc-macro expansion. That is RA's documented weak spot and the crux of
//! the S1 / D-A2 question.
//!
//! The exact token `count.get() /* BENCH_MACRO_ANCHOR */` is a stable,
//! unique anchor the harness rewrites. Do not reformat or duplicate it.

use leptos::*;

use crate::util::format::{percent, thousands};

#[component]
pub fn MetricsPanel(count: ReadSignal<i32>, target: i32) -> impl IntoView {
    let pct = create_memo(move |_| {
        if target == 0 {
            0.0
        } else {
            (count.get() as f64 / target as f64) * 100.0
        }
    });

    let remaining = move || (target - count.get()).max(0);
    let done = move || count.get() >= target;

    view! {
        <section class="metrics">
            <h3>"Metrics"</h3>
            <dl class="metrics-grid">
                <dt>"current"</dt>
                <dd class="metrics-current">
                    {move || thousands(count.get() /* BENCH_MACRO_ANCHOR */ as i64)}
                </dd>
                <dt>"target"</dt>
                <dd>{move || thousands(target as i64)}</dd>
                <dt>"progress"</dt>
                <dd>{move || percent(pct.get())}</dd>
                <dt>"remaining"</dt>
                <dd>{move || thousands(remaining() as i64)}</dd>
            </dl>
            <Show
                when=done
                fallback=move || view! { <p class="metrics-pending">"in progress"</p> }
            >
                <p class="metrics-done">"target reached"</p>
            </Show>
        </section>
    }
}
