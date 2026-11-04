//! Hand-wired Scry.Rust registry (POC step 4.1), driven from stdin.
//!
//! Dioxus signals belong to the UI thread, so the registry cannot live on the thread that reads
//! stdin. It lives in a future on the Dioxus runtime instead, and the reader thread hands it each
//! command over a channel and waits for the reply - the same marshalling Scry.NET does for WPF and
//! WinForms, and the one the endpoint will need.

use std::io::BufRead;
use std::sync::mpsc as std_mpsc;
use std::thread;

use dioxus::prelude::*;
use scry::Registry;
use tokio::sync::mpsc;

use crate::model::Board;

type Request = (String, std_mpsc::Sender<String>);

pub fn use_scry(board: Signal<Board>, uptime: Signal<u64>) {
    use_future(move || async move {
        let (requests, mut incoming) = mpsc::unbounded_channel::<Request>();
        thread::spawn(move || read_stdin(requests));

        let mut registry = Registry::new();
        registry.add("board", board).add("uptime", uptime).add_static("filter", &crate::FILTER);
        eprintln!("scry: registry ready. Try `ls board` or `set filter \"Done\"`.");

        while let Some((line, reply)) = incoming.recv().await {
            let _ = reply.send(scry::execute(&registry, &line).to_string());
        }
    });
}

fn read_stdin(requests: mpsc::UnboundedSender<Request>) {
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let (reply, response) = std_mpsc::channel();
        if requests.send((line, reply)).is_err() {
            break;
        }
        if let Ok(text) = response.recv() {
            println!("{text}");
        }
    }
}
