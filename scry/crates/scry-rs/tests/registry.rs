//! End-to-end behavior of `#[derive(Reflect)]` and `Registry`, from outside the crate - the way an
//! app uses them, which is also the only place the derive's `::scry::` paths resolve.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use scry::{Reflect, Registry, execute};
use serde_json::json;

#[derive(Reflect, Debug)]
enum Mode {
    Idle,
    Busy { job: u32 },
}

#[derive(Reflect)]
struct Item {
    name: String,
    qty: u32,
    tags: Vec<String>,
    mode: Mode,
}

#[derive(Reflect)]
struct Pair<A, B>(A, B);

#[derive(Debug)]
#[allow(dead_code)] // read only through Debug, which is the point
struct DebugOnly {
    x: i32,
}

struct NotReflect;

#[derive(Reflect)]
struct State {
    items: RwLock<Vec<Item>>,
    counter: Mutex<u64>,
    flag: AtomicBool,
    plain: u32,
    pair: Pair<u8, String>,
    debug_only: DebugOnly,
    opaque: NotReflect,
    maybe: Option<u8>,
}

static STATIC_FLAG: AtomicBool = AtomicBool::new(false);

fn registry() -> (Registry, Arc<State>) {
    let state = Arc::new(State {
        items: RwLock::new(vec![
            Item { name: "hammer".into(), qty: 3, tags: vec!["tools".into()], mode: Mode::Busy { job: 5 } },
            Item { name: "bolt".into(), qty: 100, tags: vec![], mode: Mode::Idle },
        ]),
        counter: Mutex::new(41),
        flag: AtomicBool::new(false),
        plain: 7,
        pair: Pair(1, "one".into()),
        debug_only: DebugOnly { x: 7 },
        opaque: NotReflect,
        maybe: None,
    });
    let mut registry = Registry::new();
    registry.add("state", Arc::clone(&state)).add_static("flag", &STATIC_FLAG);
    (registry, state)
}

fn error_code(result: Result<impl std::fmt::Debug, scry::Error>) -> &'static str {
    result.expect_err("expected an error").code()
}

#[test]
fn reads_nested_values_through_locks_and_collections() {
    let (registry, _) = registry();
    assert_eq!(registry.get("state.items[0].qty", 1).unwrap(), json!(3));
    assert_eq!(registry.get("state.items[0].tags[0]", 1).unwrap(), json!("tools"));
    assert_eq!(registry.get("state.counter", 1).unwrap(), json!(41));
    assert_eq!(registry.get("state.pair.1", 1).unwrap(), json!("one"));
    assert_eq!(registry.get("state.items[0].mode", 3).unwrap(), json!({ "Busy": { "job": 5 } }));
    assert_eq!(registry.get("state.items[0].mode.job", 1).unwrap(), json!(5));
    assert_eq!(registry.get("state.items[1].mode", 1).unwrap(), json!("Idle"));
    assert_eq!(registry.get("state.maybe", 1).unwrap(), json!(null));
}

#[test]
fn fields_without_reflect_fall_back_to_debug_or_type_name() {
    let (registry, _) = registry();
    assert_eq!(registry.get("state.debug_only", 1).unwrap(), json!("DebugOnly { x: 7 }"));
    let opaque = registry.get("state.opaque", 1).unwrap();
    assert!(opaque.as_str().unwrap().contains("NotReflect"), "{opaque}");
    assert_eq!(error_code(registry.get("state.debug_only.x", 1)), "not_navigable");
    assert_eq!(error_code(registry.set("state.debug_only", &json!("x"))), "not_writable");
}

#[test]
fn snapshots_are_bounded_by_depth() {
    let (registry, _) = registry();
    let shallow = registry.get("state.items", 1).unwrap();
    assert!(shallow[0].as_str().unwrap().starts_with('<'), "{shallow}");
    assert_eq!(registry.get("state.items", 2).unwrap()[0]["qty"], json!(3));
}

#[test]
fn writes_succeed_through_locks_and_atomics() {
    let (registry, state) = registry();

    registry.set("state.items[0].qty", &json!(9)).unwrap();
    assert_eq!(state.items.read().unwrap()[0].qty, 9);

    registry.set("state.items[0].mode.job", &json!(6)).unwrap();
    registry.set("state.items[1].mode", &json!("Idle")).unwrap();
    assert!(matches!(state.items.read().unwrap()[0].mode, Mode::Busy { job: 6 }));

    registry.set("state.items[1]", &json!({ "name": "nut", "qty": 1 })).unwrap();
    assert_eq!(state.items.read().unwrap()[1].name, "nut");

    registry.set("state.counter", &json!(42)).unwrap();
    assert_eq!(*state.counter.lock().unwrap(), 42);

    registry.set("state.flag", &json!(true)).unwrap();
    assert!(state.flag.load(Ordering::SeqCst));

    registry.set("flag", &json!(true)).unwrap();
    assert!(STATIC_FLAG.load(Ordering::SeqCst));
}

#[test]
fn writes_without_interior_mutability_are_refused() {
    let (registry, state) = registry();
    assert_eq!(error_code(registry.set("state.plain", &json!(1))), "not_writable");
    assert_eq!(error_code(registry.set("state.pair.0", &json!(2))), "not_writable");
    assert_eq!(state.plain, 7);
}

#[test]
fn bad_paths_and_values_are_reported() {
    let (registry, _) = registry();
    assert_eq!(error_code(registry.get("nope", 1)), "no_such_root");
    assert_eq!(error_code(registry.get("state.missing", 1)), "no_such_member");
    assert_eq!(error_code(registry.get("state.items[5]", 1)), "index_out_of_range");
    assert_eq!(error_code(registry.get("state.maybe.x", 1)), "empty");
    assert_eq!(error_code(registry.set("state.items[0].qty", &json!("x"))), "type_mismatch");
    assert_eq!(error_code(registry.set("state.items[0].mode", &json!("Busy"))), "type_mismatch");
}

#[test]
fn a_held_lock_reports_busy_instead_of_deadlocking() {
    let (registry, state) = registry();
    let _held = state.counter.lock().unwrap();
    assert_eq!(error_code(registry.get("state.counter", 1).map(|_| ())), "lock_busy");
}

#[test]
fn describe_lists_members_with_their_declared_types() {
    let (registry, _) = registry();
    let description = registry.describe("state.items[0]").unwrap();
    assert_eq!(description["kind"], json!("struct"));
    let members = description["members"].as_array().unwrap();
    assert_eq!(members[1], json!({ "name": "qty", "type": "u32" }));
}

#[test]
fn text_commands_return_json_replies() {
    let (registry, _) = registry();
    assert_eq!(execute(&registry, "get state.items[0].name"), json!({ "ok": true, "result": "hammer" }));
    assert_eq!(execute(&registry, "set state.counter 7"), json!({ "ok": true, "result": 7 }));
    assert_eq!(execute(&registry, "bogus")["error"]["code"], json!("bad_command"));
    assert_eq!(execute(&registry, "roots")["result"][1]["name"], json!("flag"));
}

#[cfg(feature = "script")]
mod scripts {
    use super::*;

    fn eval(registry: &Registry, source: &str) -> serde_json::Value {
        execute(registry, &format!("eval {source}"))
    }

    #[test]
    fn scripts_navigate_and_compute() {
        let (registry, _) = registry();
        assert_eq!(eval(&registry, "state.items[0].qty * 2 + state.counter")["result"], json!(47));
        assert_eq!(eval(&registry, "state.items.len()")["result"], json!(2));
        assert_eq!(eval(&registry, "state.items[1].mode == \"Idle\"")["result"], json!(true));
        assert_eq!(eval(&registry, "state.items[0].mode.job")["result"], json!(5));
        assert_eq!(eval(&registry, "state.maybe == ()")["result"], json!(true));
        assert_eq!(eval(&registry, "state.items[0].type_name()")["result"], json!("Item"));
        assert_eq!(eval(&registry, "root(\"state\").pair[1]")["result"], json!("one"));
        let names = "let n = []; for item in state.items { n.push(item.name) } n";
        assert_eq!(eval(&registry, names)["result"], json!(["hammer", "bolt"]));
    }

    #[test]
    fn a_node_result_is_a_snapshot() {
        let (registry, _) = registry();
        assert_eq!(eval(&registry, "state.items[1]")["result"]["name"], json!("bolt"));
        assert_eq!(eval(&registry, "state.items[1].get()")["result"]["qty"], json!(100));
    }

    #[test]
    fn scripts_write_through_the_registry_rules() {
        let (registry, state) = registry();
        let reply = eval(&registry, "state.items[0].qty += 1; state.counter = 0; flag = true; state.items[0].qty");
        assert_eq!(reply, json!({ "ok": true, "result": 4 }));
        assert_eq!(*state.counter.lock().unwrap(), 0);
        assert!(STATIC_FLAG.load(Ordering::SeqCst));

        eval(&registry, "let item = state.items[1]; item.name = \"nut\"; item.mode = \"Idle\"");
        assert_eq!(state.items.read().unwrap()[1].name, "nut");

        assert_eq!(eval(&registry, "state.plain = 1")["error"]["code"], json!("not_writable"));
        assert_eq!(state.plain, 7);
    }

    #[test]
    fn failures_keep_their_codes_and_prints_are_returned() {
        let (registry, _) = registry();
        let reply = eval(&registry, "print(\"before\"); state.missing");
        assert_eq!(reply["error"]["code"], json!("no_such_member"));
        assert_eq!(reply["output"], json!(["before"]));
        assert_eq!(eval(&registry, "let x = ;")["error"]["code"], json!("script_error"));
        assert_eq!(eval(&registry, "import \"x\" as y; 1")["error"]["code"], json!("script_error"));
    }

    #[test]
    fn long_sequences_are_counted_and_iterated_in_full() {
        let (registry, state) = registry();
        state.items.write().unwrap()[0].tags = (0..250).map(|n| n.to_string()).collect();
        assert_eq!(eval(&registry, "state.items[0].tags.len()")["result"], json!(250));
        assert_eq!(eval(&registry, "let n = 0; for tag in state.items[0].tags { n += 1 } n")["result"], json!(250));
    }

    #[test]
    fn a_runaway_script_is_stopped() {
        let (registry, _) = registry();
        assert_eq!(eval(&registry, "loop {}")["error"]["code"], json!("script_limit"));
    }
}

#[cfg(feature = "script")]
#[test]
fn a_root_without_members_is_a_plain_variable_written_back_at_the_end() {
    let (mut registry, state) = registry();
    let level = Arc::new(Mutex::new(1u32));
    registry.add("level", Arc::clone(&level));
    assert_eq!(execute(&registry, "eval if level == 1 { level = level + 1; } level")["result"], json!(2));
    assert_eq!(*level.lock().unwrap(), 2);
    assert_eq!(execute(&registry, "eval let level = 9; level")["result"], json!(9));
    assert_eq!(*level.lock().unwrap(), 2, "a shadowing `let` is not written back");
    assert_eq!(execute(&registry, "eval level = 7; state.missing")["error"]["code"], json!("no_such_member"));
    assert_eq!(*level.lock().unwrap(), 2, "nothing is written back after an error");
    assert_eq!(execute(&registry, "eval level = \"x\"")["error"]["code"], json!("type_mismatch"));
    assert_eq!(execute(&registry, "eval state.counter = 3; state.counter")["result"], json!(3));
    assert_eq!(*state.counter.lock().unwrap(), 3);
}

/// A value whose snapshot panics, as a buggy `Debug` impl might.
struct Bomb;

impl Reflect for Bomb {
    fn type_name(&self) -> &'static str {
        "Bomb"
    }

    fn kind(&self) -> scry::Kind {
        scry::Kind::Scalar
    }

    fn snapshot(&self, _depth: usize) -> serde_json::Value {
        panic!("bomb went off")
    }

    fn read(&self, _path: &[scry::Segment], visit: scry::Visit<'_>) -> Result<(), scry::Error> {
        visit(self)
    }

    fn write(&mut self, _path: &[scry::Segment], visit: scry::VisitMut<'_>) -> Result<(), scry::Error> {
        visit(self)
    }
}

#[test]
fn a_panic_is_an_error_and_leaves_the_apps_lock_usable() {
    let bomb = Arc::new(Mutex::new(Bomb));
    let mut registry = Registry::new();
    registry.add("bomb", Arc::clone(&bomb)).add("count", Arc::new(Mutex::new(5u8)));
    let reply = execute(&registry, "get bomb");
    assert_eq!(reply["error"]["code"], json!("panicked"), "{reply}");
    assert!(reply["error"]["message"].as_str().unwrap().contains("bomb went off"));
    assert!(!bomb.is_poisoned(), "the registry's guard must not poison the app's lock");
    assert_eq!(execute(&registry, "get count")["result"], json!(5));
}

#[cfg(feature = "dioxus")]
#[test]
fn a_global_signal_outside_the_runtime_is_an_error_not_a_panic() {
    use dioxus_signals::{GlobalSignal, Signal};
    static GLOBAL: GlobalSignal<u8> = Signal::global(|| 1);
    let mut registry = Registry::new();
    registry.add_static("global", &GLOBAL);
    assert_eq!(execute(&registry, "get global")["error"]["code"], json!("no_runtime"));
    assert_eq!(execute(&registry, "set global 2")["error"]["code"], json!("no_runtime"));
}
