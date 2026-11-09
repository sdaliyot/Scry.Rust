//! A small, long-running console app for Scry.Rust to attach to.
//!
//! It is shaped to exercise what a registry has to handle, not to be useful on its own:
//! process-wide state behind a static root, private fields in other modules, generic functions and
//! types used with several concrete type arguments, a list of trait objects, a togglable static
//! flag, and a worker thread that keeps mutating all of it so live inspection has something to see.

mod inventory;
mod pricing;
mod routes;
mod util;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use inventory::{Category, Inventory};
use pricing::{CategoryMarkup, Discount, PriceRule};
use util::{Cache, Describe, clamp_all, describe};

/// The registry's natural root: everything interesting is reachable from here.
static APP: OnceLock<Arc<AppState>> = OnceLock::new();

/// A plain static flag the worker reads on every sale - the simplest thing to flip from outside.
static DISCOUNTS_ENABLED: AtomicBool = AtomicBool::new(true);

pub struct AppState {
    inventory: RwLock<Inventory>,
    stats: Mutex<Stats>,
    rules: Mutex<Vec<Box<dyn PriceRule + Send>>>,
    labels: Mutex<Cache<u32, String>>,
    session: parking_lot::RwLock<Session>,
    /// Sales whose label matches one of these are not shown as the last sale.
    quiet: parking_lot::RwLock<Vec<regex::Regex>>,
    started: Instant,
}

/// Who is running the shop, and since when.
struct Session {
    id: uuid::Uuid,
    opened: chrono::DateTime<chrono::Utc>,
    cashier: Option<String>,
}

impl AppState {
    fn new() -> Self {
        Self {
            inventory: RwLock::new(Inventory::seeded()),
            stats: Mutex::new(Stats::default()),
            rules: Mutex::new(vec![
                Box::new(Discount::new("weekday", 10)),
                Box::new(CategoryMarkup::new(Category::Tools, 25)),
            ]),
            labels: Mutex::new(Cache::new(8)),
            session: parking_lot::RwLock::new(Session {
                id: uuid::Uuid::new_v4(),
                opened: chrono::Utc::now(),
                cashier: None,
            }),
            quiet: parking_lot::RwLock::new(vec![regex::Regex::new("^Nothing$").expect("a valid pattern")]),
            started: Instant::now(),
        }
    }
}

#[derive(Debug, Default)]
struct Stats {
    ticks: u64,
    sold: u64,
    revenue_cents: i64,
    last_sale: Option<String>,
}

impl Describe for Stats {
    fn describe(&self) -> String {
        format!("{} ticks, {} sold, revenue {}", self.ticks, self.sold, util::format_cents(self.revenue_cents))
    }
}

fn main() {
    let state = APP.get_or_init(|| Arc::new(AppState::new()));
    println!("console-demo running, pid {}. Ctrl+C to stop.", std::process::id());
    {
        let session = state.session.read();
        println!("session {} opened {}", session.id, session.opened.format("%Y-%m-%d %H:%M:%S UTC"));
        println!("{}", util::tag_label(1, "first run"));
    }

    let worker_state = Arc::clone(state);
    thread::spawn(move || worker_loop(&worker_state));

    loop {
        thread::sleep(Duration::from_secs(5));
        report(state);
    }
}

/// Sells one unit per second and restocks every tenth tick. The pseudo-random seed is a local:
/// it exists only in this stack frame, which is exactly what a registry cannot reach.
fn worker_loop(state: &AppState) {
    let mut seed: u64 = 0x5eed;
    loop {
        thread::sleep(Duration::from_secs(1));
        seed = util::next_random(seed);

        let sold = {
            let mut inventory = state.inventory.write().unwrap();
            let index = (seed % inventory.len() as u64) as usize;
            let item = inventory.sell_one(index);
            if state.stats.lock().unwrap().ticks % 10 == 9 {
                inventory.restock_empty();
            }
            item
        };

        let mut stats = state.stats.lock().unwrap();
        stats.ticks += 1;
        routes::serve(stats.ticks);
        let Some(item) = sold else { continue };

        let mut price = item.price_cents();
        if DISCOUNTS_ENABLED.load(Ordering::Relaxed) {
            for rule in state.rules.lock().unwrap().iter() {
                price = rule.apply(price, &item);
            }
        }
        let mut bounded = [price];
        clamp_all(&mut bounded, 1, 1_000_000);

        let label = {
            let mut labels = state.labels.lock().unwrap();
            labels.get(&item.id).unwrap_or_else(|| {
                let label = describe(&item);
                labels.put(item.id, label.clone());
                label
            })
        };

        stats.sold += 1;
        stats.revenue_cents += bounded[0];
        if !state.quiet.read().iter().any(|pattern| pattern.is_match(&label)) {
            stats.last_sale = Some(format!("{label} for {}", util::format_cents(bounded[0])));
        }
    }
}

fn report(state: &AppState) {
    let stats = state.stats.lock().unwrap();
    let inventory = state.inventory.read().unwrap();
    let mut fill = inventory.fill_ratios();
    clamp_all(&mut fill, 0.0, 1.0);
    let average_fill = fill.iter().sum::<f64>() / fill.len().max(1) as f64;
    let rules = if DISCOUNTS_ENABLED.load(Ordering::Relaxed) {
        let rules = state.rules.lock().unwrap();
        rules.iter().map(|rule| rule.name().to_owned()).collect::<Vec<_>>().join(", ")
    } else {
        "off".to_owned()
    };

    println!(
        "[{:>4}s] {} | stock value {} | avg fill {:.0}% | rules: {} | last: {} | cashier: {}",
        state.started.elapsed().as_secs(),
        describe(&*stats),
        util::format_cents(inventory.total_value_cents()),
        average_fill * 100.0,
        rules,
        stats.last_sale.as_deref().unwrap_or("-"),
        state.session.read().cashier.as_deref().unwrap_or("nobody"),
    );
}
