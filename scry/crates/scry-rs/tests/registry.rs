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
