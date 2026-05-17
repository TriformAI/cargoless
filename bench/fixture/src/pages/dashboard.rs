//! Dashboard page: todo list + profile form side by side.

use leptos::*;

use crate::components::form::ProfileForm;
use crate::components::todo::TodoList;

#[component]
pub fn DashboardPage() -> impl IntoView {
    view! {
        <div class="page page-dashboard">
            <header class="page-head">
                <h2>"Dashboard"</h2>
            </header>
            <div class="page-cols">
                <TodoList/>
                <ProfileForm/>
            </div>
        </div>
    }
}
