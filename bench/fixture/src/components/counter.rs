//! A small but non-trivial counter with bounded range, step control, and a
//! derived parity label. Exercises signal read/write, memos, and a couple of
//! `view!` blocks.

use leptos::*;

#[derive(Clone, Copy)]
pub struct CounterBounds {
    pub min: i32,
    pub max: i32,
}

impl Default for CounterBounds {
    fn default() -> Self {
        CounterBounds { min: -50, max: 50 }
    }
}

#[component]
pub fn Counter(#[prop(optional)] bounds: Option<CounterBounds>) -> impl IntoView {
    let bounds = bounds.unwrap_or_default();
    let (value, set_value) = create_signal(0i32);
    let (step, set_step) = create_signal(1i32);

    // `bounds`, signals are all `Copy`, so each handler captures its own
    // copy — no shared-closure move conflicts.
    let inc = move |_| {
        set_value.update(|v| {
            *v = (*v + step.get_untracked()).max(bounds.min).min(bounds.max)
        })
    };
    let dec = move |_| {
        set_value.update(|v| {
            *v = (*v - step.get_untracked()).max(bounds.min).min(bounds.max)
        })
    };
    let reset = move |_| set_value.set(0);

    let parity = create_memo(move |_| {
        if value.get() % 2 == 0 {
            "even"
        } else {
            "odd"
        }
    });

    view! {
        <section class="counter">
            <h3>"Counter"</h3>
            <div class="counter-readout">
                <span class="counter-value">{move || value.get()}</span>
                <span class="counter-parity">"(" {move || parity.get()} ")"</span>
            </div>
            <div class="counter-controls">
                <button
                    on:click=dec
                    prop:disabled=move || value.get() <= bounds.min
                >
                    "-"
                </button>
                <button on:click=reset>"reset"</button>
                <button
                    on:click=inc
                    prop:disabled=move || value.get() >= bounds.max
                >
                    "+"
                </button>
            </div>
            <label class="counter-step">
                "step: "
                <input
                    type="number"
                    prop:value=move || step.get()
                    on:input=move |ev| {
                        let raw = event_target_value(&ev);
                        if let Ok(n) = raw.parse::<i32>() {
                            set_step.set(n.max(1));
                        }
                    }
                />
            </label>
            <Show
                when=move || value.get() >= bounds.max
                fallback=|| view! { <span/> }
            >
                <p class="counter-warn">"at maximum"</p>
            </Show>
        </section>
    }
}
