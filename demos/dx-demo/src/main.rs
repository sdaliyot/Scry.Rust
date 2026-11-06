//! A small Dioxus desktop app for Scry.Rust to attach to.
//!
//! The UI-side counterpart of console-demo. Its state lives in Dioxus signals rather than in
//! mutexes: a global signal (a static root, like console-demo's `APP`), a context-provided signal
//! holding the board, and component-local signals, one of them updated by a background future.
//! Ordinary signals belong to the UI thread, so an endpoint running on another thread will have to
//! marshal onto it to read or write them - the Rust equivalent of Scry.NET's WPF/WinForms adapters.
//! It also has a router with a few pages, to switch between from outside (`scry set ui.route ...`).

mod model;

use std::time::Duration;

use dioxus::prelude::*;
use model::{Board, Priority};

/// Which tasks the list shows. Global, so it is reachable without walking the component tree.
static FILTER: GlobalSignal<Filter> = Signal::global(|| Filter::All);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Filter {
    All,
    Open,
    Done,
}

impl Filter {
    fn matches(self, task: &model::Task) -> bool {
        match self {
            Filter::All => true,
            Filter::Open => !task.done,
            Filter::Done => task.done,
        }
    }
}

/// The pages. `ui.route` reads and sets these as URLs (`/tasks/3`).
#[derive(Routable, Clone, PartialEq)]
enum Route {
    #[route("/")]
    Tasks {},
    #[route("/tasks/:id")]
    TaskDetail { id: u32 },
    #[route("/about")]
    About {},
}

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    let board = use_context_provider(|| Signal::new(Board::seeded()));
    let mut uptime = use_signal(|| 0u64);

    use_future(move || async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            uptime += 1;
        }
    });

    let pid = std::process::id();
    let summary = board.read().summary();

    rsx! {
        div { style: "font-family: 'Segoe UI', sans-serif; padding: 16px;",
            h2 { "Scry.Rust dx demo" }
            p { "pid {pid} · up {uptime}s · {summary}" }
            Router::<Route> {}
        }
    }
}

/// Links to the pages, above each of them.
#[component]
fn Nav() -> Element {
    rsx! {
        nav { style: "margin-bottom: 8px; display: flex; gap: 12px;",
            Link { to: Route::Tasks {}, "Tasks" }
            Link { to: Route::About {}, "About" }
        }
    }
}

#[component]
fn Tasks() -> Element {
    let mut board = use_context::<Signal<Board>>();
    let mut draft = use_signal(String::new);
    rsx! {
        Nav {}
        div {
            input {
                value: "{draft}",
                placeholder: "New task",
                oninput: move |event| draft.set(event.value()),
            }
            button {
                onclick: move |_| {
                    let title = draft.read().trim().to_owned();
                    if !title.is_empty() {
                        board.write().add(&title, Priority::Normal);
                        draft.set(String::new());
                    }
                },
                "Add"
            }
        }
        FilterBar {}
        TaskList {}
    }
}

#[component]
fn TaskDetail(id: u32) -> Element {
    let board = use_context::<Signal<Board>>();
    let task = board.read().tasks().iter().find(|task| task.id == id).cloned();
    rsx! {
        Nav {}
        match task {
            Some(task) => rsx! {
                h3 { "{task.title}" }
                p { "Priority {task.priority:?} · " if task.done { "done" } else { "open" } }
            },
            None => rsx! { p { "No task {id}." } },
        }
    }
}

#[component]
fn About() -> Element {
    rsx! {
        Nav {}
        p { "A Dioxus desktop app for Scry.Rust to attach to." }
    }
}

#[component]
fn FilterBar() -> Element {
    let current = *FILTER.read();
    rsx! {
        div { style: "margin: 8px 0;",
            for filter in [Filter::All, Filter::Open, Filter::Done] {
                button {
                    key: "{filter:?}",
                    disabled: current == filter,
                    onclick: move |_| *FILTER.write() = filter,
                    "{filter:?}"
                }
            }
        }
    }
}

#[component]
fn TaskList() -> Element {
    let mut board = use_context::<Signal<Board>>();
    let filter = *FILTER.read();
    let visible: Vec<model::Task> = board.read().tasks().iter().filter(|task| filter.matches(task)).cloned().collect();

    rsx! {
        ul {
            for task in visible {
                li { key: "{task.id}",
                    input {
                        r#type: "checkbox",
                        checked: task.done,
                        onchange: move |_| board.write().toggle(task.id),
                    }
                    " "
                    Link { to: Route::TaskDetail { id: task.id }, "{task.title}" }
                    " ({task.priority:?})"
                }
            }
        }
    }
}
