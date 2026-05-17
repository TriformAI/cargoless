//! A todo list with add / toggle / remove / filter. Realistic signal-of-Vec
//! state, keyed `For`, and several `view!` blocks — a fair amount of macro
//! expansion for rust-analyzer to chew through.

use leptos::*;

use crate::domain::model::{Priority, TodoItem};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    All,
    Active,
    Done,
}

impl Filter {
    fn keep(&self, item: &TodoItem) -> bool {
        match self {
            Filter::All => true,
            Filter::Active => !item.done,
            Filter::Done => item.done,
        }
    }
}

#[component]
pub fn TodoList() -> impl IntoView {
    let items = create_rw_signal(seed_items());
    let (draft, set_draft) = create_signal(String::new());
    let (filter, set_filter) = create_signal(Filter::All);
    let next_id = create_rw_signal(100u64);

    let add = move |_| {
        let title = draft.get().trim().to_string();
        if title.is_empty() {
            return;
        }
        let id = next_id.get();
        next_id.update(|n| *n += 1);
        items.update(|list| list.push(TodoItem::new(id, &title, Priority::Normal)));
        set_draft.set(String::new());
    };

    let toggle = move |id: u64| {
        items.update(|list| {
            if let Some(it) = list.iter_mut().find(|i| i.id == id) {
                it.done = !it.done;
            }
        });
    };

    let remove = move |id: u64| {
        items.update(|list| list.retain(|i| i.id != id));
    };

    let visible = create_memo(move |_| {
        let f = filter.get();
        items
            .get()
            .into_iter()
            .filter(|i| f.keep(i))
            .collect::<Vec<_>>()
    });

    let remaining = move || items.get().iter().filter(|i| !i.done).count();

    view! {
        <section class="todo">
            <h3>"Todo"</h3>
            <form
                class="todo-add"
                on:submit=move |ev| {
                    ev.prevent_default();
                    add(());
                }
            >
                <input
                    class="todo-input"
                    placeholder="what needs doing?"
                    prop:value=move || draft.get()
                    on:input=move |ev| set_draft.set(event_target_value(&ev))
                />
                <button type="submit">"add"</button>
            </form>

            <div class="todo-filters">
                <For
                    each=move || [Filter::All, Filter::Active, Filter::Done]
                    key=|f| format!("{f:?}")
                    let:f
                >
                    <button
                        class="todo-filter"
                        class:active=move || filter.get() == f
                        on:click=move |_| set_filter.set(f)
                    >
                        {format!("{f:?}")}
                    </button>
                </For>
            </div>

            <ul class="todo-list">
                <For
                    each=move || visible.get()
                    key=|i| i.id
                    let:item
                >
                    {
                        // Destructure once into owned/Copy locals so no
                        // single `item` is moved into several closures.
                        let id = item.id;
                        let is_done = item.done;
                        let title = item.title.clone();
                        let prio = item.priority.tag();
                        view! {
                            <li class="todo-row" class:done=move || is_done>
                                <input
                                    type="checkbox"
                                    prop:checked=is_done
                                    on:change=move |_| toggle(id)
                                />
                                <span class="todo-title">{title}</span>
                                <span class="todo-prio">{prio}</span>
                                <button
                                    class="todo-del"
                                    on:click=move |_| remove(id)
                                >
                                    "x"
                                </button>
                            </li>
                        }
                    }
                </For>
            </ul>

            <p class="todo-count">
                {move || format!("{} remaining", remaining())}
            </p>
        </section>
    }
}

fn seed_items() -> Vec<TodoItem> {
    vec![
        TodoItem::new(1, "wire fs watcher", Priority::High),
        TodoItem::new(2, "rust-analyzer supervisor", Priority::High),
        TodoItem::new(3, "green/red model", Priority::Normal),
        TodoItem::new(4, "dev server holding page", Priority::Normal),
        TodoItem::new(5, "write the launch blog", Priority::Low),
    ]
}
