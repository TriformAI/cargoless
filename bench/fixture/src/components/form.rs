//! A validated sign-up-ish form. Pulls real validation logic from the domain
//! layer so there is a non-trivial type/trait surface behind the `view!`.

use leptos::*;

use crate::domain::validation::{validate_profile, ProfileDraft, ValidationReport};

#[component]
pub fn ProfileForm() -> impl IntoView {
    let (name, set_name) = create_signal(String::new());
    let (email, set_email) = create_signal(String::new());
    let (age, set_age) = create_signal(String::new());
    let (submitted, set_submitted) = create_signal(false);

    let report = create_memo(move |_| -> ValidationReport {
        let draft = ProfileDraft {
            name: name.get(),
            email: email.get(),
            age: age.get(),
        };
        validate_profile(&draft)
    });

    // A `Memo` is `Copy`, so it can be read from several `view!` closures
    // without move conflicts (a plain closure could not).
    let valid = create_memo(move |_| report.get().is_ok());

    // Pre-collect error <li>s outside the macro (no `<For>`/`let:` — kept
    // to the conservative leptos-0.6.15 rsx subset that parses reliably).
    let error_items = move || {
        report
            .get()
            .errors
            .into_iter()
            .map(|e| view! { <li>{e}</li> })
            .collect::<Vec<_>>()
    };

    view! {
        <section class="form">
            <h3>"Profile"</h3>
            <form
                on:submit=move |ev| {
                    ev.prevent_default();
                    set_submitted.set(true);
                }
            >
                <label class="field">
                    "name"
                    <input
                        prop:value=move || name.get()
                        on:input=move |ev| set_name.set(event_target_value(&ev))
                    />
                </label>
                <label class="field">
                    "email"
                    <input
                        prop:value=move || email.get()
                        on:input=move |ev| set_email.set(event_target_value(&ev))
                    />
                </label>
                <label class="field">
                    "age"
                    <input
                        type="number"
                        prop:value=move || age.get()
                        on:input=move |ev| set_age.set(event_target_value(&ev))
                    />
                </label>

                <button type="submit" prop:disabled=move || !valid.get()>
                    "save"
                </button>
            </form>

            <Show
                when=move || !report.get().errors.is_empty()
                fallback=|| view! { <p class="form-ok">"looks good"</p> }
            >
                <ul class="form-errors">{error_items}</ul>
            </Show>

            <Show
                when=move || submitted.get() && valid.get()
                fallback=|| ()
            >
                <p class="form-saved">"saved"</p>
            </Show>
        </section>
    }
}
