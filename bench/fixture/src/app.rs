//! Top-level application shell. Owns the "current view" selector (a
//! hand-rolled router-free navigation so the fixture needs only the `leptos`
//! crate) and stitches the pages together.

use leptos::*;

use crate::components::nav::NavBar;
use crate::pages::about::AboutPage;
use crate::pages::dashboard::DashboardPage;
use crate::pages::home::HomePage;

/// Which top-level page is shown. Router-free on purpose (fewer external
/// crates ⇒ a tighter determinism surface for the benchmark).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Home,
    Dashboard,
    About,
}

impl Route {
    pub fn label(&self) -> &'static str {
        match self {
            Route::Home => "Home",
            Route::Dashboard => "Dashboard",
            Route::About => "About",
        }
    }

    pub fn all() -> [Route; 3] {
        [Route::Home, Route::Dashboard, Route::About]
    }
}

#[component]
pub fn App() -> impl IntoView {
    let route = create_rw_signal(Route::Home);

    let body = move || match route.get() {
        Route::Home => view! { <HomePage/> }.into_view(),
        Route::Dashboard => view! { <DashboardPage/> }.into_view(),
        Route::About => view! { <AboutPage/> }.into_view(),
    };

    view! {
        <div class="app-shell">
            <NavBar route=route/>
            <main class="app-main">
                {body}
            </main>
            <footer class="app-footer">
                <span>"cargoless reference fixture — S1 / AC#2"</span>
            </footer>
        </div>
    }
}
