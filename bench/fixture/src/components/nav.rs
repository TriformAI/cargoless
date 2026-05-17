//! Navigation bar. Drives the router-free `Route` selector in `app`.

use leptos::*;

use crate::app::Route;

#[component]
pub fn NavBar(route: RwSignal<Route>) -> impl IntoView {
    view! {
        <nav class="navbar">
            <ul class="navbar-list">
                <For
                    each=move || Route::all().into_iter().collect::<Vec<_>>()
                    key=|r| r.label()
                    let:r
                >
                    {
                        let is_active = move || route.get() == r;
                        view! {
                            <li
                                class="navbar-item"
                                class:active=is_active
                            >
                                <button
                                    class="navbar-link"
                                    on:click=move |_| route.set(r)
                                >
                                    {r.label()}
                                </button>
                            </li>
                        }
                    }
                </For>
            </ul>
        </nav>
    }
}
