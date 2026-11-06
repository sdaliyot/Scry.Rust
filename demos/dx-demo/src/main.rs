//! A small Dioxus desktop app for Scry.Rust to attach to.
//!
//! The UI-side counterpart of console-demo. Its state lives in Dioxus signals rather than in
//! mutexes: a global signal (a static root, like console-demo's `APP`), a context-provided signal
//! holding the board, and component-local signals, one of them updated by a background future.
//! Ordinary signals belong to the UI thread, so an endpoint running on another thread will have to
//! marshal onto it to read or write them - the Rust equivalent of Scry.NET's WPF/WinForms adapters.
//! It also has a router with a few pages, to switch between from outside (`scry set ui.route ...`),
//! and opens a second window (About, "Open a notes window") for `scry windows`. About also loads the
//! board's stats with `use_resource` and clears finished tasks with `use_action`.

// `#[derive(Routable)]` writes `child: child` for a `#[child]` route.
#![allow(clippy::redundant_field_names)]

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
    /// A child route type: its routes live under `/settings`.
    #[child("/settings")]
    Settings { child: SettingsRoute },
}

/// The settings pages, nested under `/settings`.
#[derive(Routable, Clone, PartialEq)]
enum SettingsRoute {
    #[route("/")]
    General {},
    #[route("/theme/:name")]
    Theme { name: String },
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
            Link { to: Route::Settings { child: SettingsRoute::General {} }, "Settings" }
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
    let mut board = use_context::<Signal<Board>>();
    // Loaded in the background, and again whenever the board changes.
    let stats = use_resource(move || async move {
        let summary = board.read().summary();
        tokio::time::sleep(Duration::from_millis(300)).await;
        summary
    });
    // Runs only when asked to.
    let mut clear_done = use_action(move || async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok::<usize, dioxus::CapturedError>(board.write().clear_done())
    });
    rsx! {
        Nav {}
        p { "A Dioxus desktop app for Scry.Rust to attach to." }
        p {
            match &*stats.read() {
                Some(summary) => rsx! { "Board: {summary}" },
                None => rsx! { "Loading the board's stats…" },
            }
        }
        button { onclick: move |_| { clear_done.call(); }, "Clear finished tasks" }
        if clear_done.pending() {
            span { " clearing…" }
        } else if let Some(Ok(removed)) = clear_done.value() {
            span { " cleared {removed}" }
        }
        button {
            onclick: move |_| {
                dioxus::desktop::window().new_window(VirtualDom::new(Notes), dioxus::desktop::Config::new());
            },
            "Open a notes window"
        }
    }
}

#[component]
fn General() -> Element {
    rsx! {
        Nav {}
        h3 { "Settings" }
        for name in ["light", "dark"] {
            p {
                Link { to: Route::Settings { child: SettingsRoute::Theme { name: name.to_owned() } }, "Theme: {name}" }
            }
        }
    }
}

#[component]
fn Theme(name: String) -> Element {
    rsx! {
        Nav {}
        h3 { "Theme {name}" }
    }
}

/// The root of a second window: a virtual dom of its own, with its own state.
#[component]
fn Notes() -> Element {
    let mut note = use_signal(String::new);
    rsx! {
        div { style: "font-family: 'Segoe UI', sans-serif; padding: 16px;",
            h2 { "Notes" }
            textarea { rows: 4, value: "{note}", oninput: move |event| note.set(event.value()) }
            p { "{note.read().len()} characters" }
            button { onclick: move |_| dioxus::desktop::window().close(), "Close" }
        }
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
