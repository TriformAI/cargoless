//! Navigation bar. Drives the router-free `Route` selector in `app`.
//!
//! Renders the route list by pre-collecting a `Vec<_>` of views in plain
//! Rust and interpolating it (`{items}`) instead of `<For>`: keeps
//! turbofish/iterator code OUT of the `view!` macro, which the leptos
//! 0.6.15 rsx parser rejects in attribute position.

use leptos::*;

use crate::app::Route;

#[component]
pub fn NavBar(route: RwSignal<Route>) -> impl IntoView {
    let items = move || {
        Route::all()
            .into_iter()
            .map(|r| {
                let is_active = move || route.get() == r;
                view! {
                    <li class="navbar-item" class:active=is_active>
                        <button
                            class="navbar-link"
                            on:click=move |_| route.set(r)
                        >
                            {r.label()}
                        </button>
                    </li>
                }
            })
            .collect::<Vec<_>>()
    };

    view! {
        <nav class="navbar">
            <ul class="navbar-list">{items}</ul>
        </nav>
    }
}
