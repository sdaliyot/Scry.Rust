//! A small web server for Scry.Rust to attach to: a notes service on actix-web.
//!
//! Its handlers sit under actix-web's route macros (`#[get]`, `#[post]`), which turn each one into
//! a service type. `cargo scry` knows nothing about them: it tries a call shim and a hook point for
//! every handler, and keeps whatever compiles. Here that is the hook points - a hook sees each
//! request, and can change it - while the handlers themselves can't be called by name; the plain
//! functions they call can. `list_notes` returns `impl Responder`, as handlers usually do.
//!
//! ```text
//! cargo scry run -p web-demo                  # listens on 127.0.0.1:8471, or $WEB_DEMO_PORT
//! curl -X POST -d "buy milk" localhost:8471/notes
//! curl localhost:8471/notes
//! ```

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use actix_web::{App, HttpResponse, HttpServer, Responder, get, post, web};

/// The notes, oldest first. Shared by every worker thread.
static NOTES: Mutex<Vec<Note>> = Mutex::new(Vec::new());

/// Requests served, by every handler.
static REQUESTS: AtomicU64 = AtomicU64::new(0);

pub struct Note {
    id: usize,
    text: String,
}

/// Adds a note and returns its id. What `POST /notes` does, and callable on its own.
pub fn add(text: &str) -> usize {
    let mut notes = NOTES.lock().unwrap();
    let id = notes.len() + 1;
    notes.push(Note { id, text: text.trim().to_owned() });
    id
}

/// Every note, one per line.
pub fn listing() -> String {
    NOTES.lock().unwrap().iter().map(|note| format!("{}: {}\n", note.id, note.text)).collect()
}

#[get("/notes")]
async fn list_notes() -> impl Responder {
    REQUESTS.fetch_add(1, Ordering::Relaxed);
    listing()
}

#[post("/notes")]
async fn add_note(body: String) -> HttpResponse {
    REQUESTS.fetch_add(1, Ordering::Relaxed);
    if body.trim().is_empty() {
        return HttpResponse::BadRequest().body("a note needs text");
    }
    HttpResponse::Created().body(add(&body).to_string())
}

#[get("/notes/{id}")]
async fn get_note(id: web::Path<usize>) -> HttpResponse {
    REQUESTS.fetch_add(1, Ordering::Relaxed);
    let notes = NOTES.lock().unwrap();
    match notes.iter().find(|note| note.id == *id) {
        Some(note) => HttpResponse::Ok().body(note.text.clone()),
        None => HttpResponse::NotFound().body(format!("no note {id}")),
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("WEB_DEMO_PORT").ok().and_then(|port| port.parse().ok()).unwrap_or(8471);
    add("try `scry-rs get NOTES`");
    println!("web-demo listening on http://127.0.0.1:{port}, pid {}", std::process::id());
    HttpServer::new(|| App::new().service(list_notes).service(add_note).service(get_note))
        .workers(2)
        .bind(("127.0.0.1", port))?
        .run()
        .await
}
