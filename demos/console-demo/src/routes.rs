//! Functions under attribute macros that rewrite them, as a web framework's route macros do (see
//! `demo-macros`). `cargo scry` knows none of these macros: it tries a call shim and a hook point
//! for each function, and leaves out whichever doesn't compile.

use demo_macros::{plain_body, route, twice};

/// Becomes a type: hookable, not callable by name.
#[route]
pub fn price_tag(cents: u64) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
}

/// Compiled twice, as `restock_note` and `restock_note_again`: one hook sees both.
#[twice]
pub fn restock_note(item: &str) -> String {
    format!("restock {item}")
}

/// Takes no hook point: callable, not hookable.
#[plain_body]
pub fn is_even(count: u64) -> bool {
    count.is_multiple_of(2)
}

/// One request to each, as the worker makes on every tick.
pub fn serve(tick: u64) -> usize {
    let tag = price_tag::handle(tick * 125);
    let note = restock_note("hammer");
    let again = restock_note_again("bolt");
    usize::from(is_even(tick)) + tag.len() + note.len() + again.len()
}
