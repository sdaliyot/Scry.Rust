//! Hand-wired Scry.Rust registry (POC step 4.1), driven from stdin.
//!
//! Each line typed into the console is a registry command - `roots`, `ls <path>`,
//! `get <path> [depth]`, `set <path> <json>` - answered with a JSON line. The endpoint in a later
//! step replaces stdin with a local socket and a CLI; the generator replaces this file.

use std::io::BufRead;
use std::thread;

use scry::Registry;

pub fn start() {
    thread::spawn(|| {
        // Every root here is a static and thread-safe, so the registry can live on this thread.
        let mut registry = Registry::new();
        registry.add_static("app", &crate::APP).add_static("discounts_enabled", &crate::DISCOUNTS_ENABLED);
        eprintln!("scry: registry ready. Try `ls app` or `get app.inventory.items[0]`.");

        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if !line.trim().is_empty() {
                println!("{}", scry::execute(&registry, &line));
            }
        }
    });
}
