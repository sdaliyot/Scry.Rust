//! Types Scry has no impl for, handled through the traits they do implement, and the lock
//! abstractions it supports.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::net::IpAddr;
use std::rc::Rc;
use std::str::FromStr;

use scry::{Reflect, Registry, execute};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Like a third-party type with serde support (a date, a money amount): no `Reflect`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Stamp {
    year: u16,
    zone: String,
}

/// Like a third-party type with only `Display`/`FromStr` (an id, a version).
#[derive(Debug, PartialEq)]
struct Tag(String);

impl FromStr for Tag {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        if text.starts_with('#') { Ok(Tag(text.to_owned())) } else { Err(format!("'{text}' does not start with #")) }
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An app type holding a foreign one, as most do.
#[derive(Reflect)]
struct Meta {
    tag: Tag,
}

#[derive(Reflect)]
struct Job {
    id: u32,
    done: parking_lot::Mutex<bool>,
}

/// Like a third-party collection: iterable by reference, nothing else.
struct Bag(Vec<Job>);

impl<'a> IntoIterator for &'a Bag {
    type Item = &'a Job;
    type IntoIter = std::slice::Iter<'a, Job>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Like a third-party map: iterable as key-value pairs.
struct Index(std::collections::BTreeMap<String, Job>);

impl<'a> IntoIterator for &'a Index {
    type Item = (&'a String, &'a Job);
    type IntoIter = std::collections::btree_map::Iter<'a, String, Job>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

fn job(id: u32) -> Job {
    Job { id, done: parking_lot::Mutex::new(false) }
}

#[derive(Reflect)]
struct State {
    bag: Bag,
    index: Index,
    stamp: parking_lot::Mutex<Stamp>,
    meta: parking_lot::Mutex<Meta>,
    address: parking_lot::RwLock<IpAddr>,
    queue: VecDeque<Job>,
    seen: HashSet<u32>,
    shared: Rc<u32>,
    counter: tokio::sync::Mutex<u64>,
    settings: tokio::sync::RwLock<Vec<String>>,
}

fn registry() -> (Registry, Rc<State>) {
    let state = Rc::new(State {
        bag: Bag(vec![job(10), job(11)]),
        index: Index([("first".to_owned(), job(20))].into()),
        stamp: parking_lot::Mutex::new(Stamp { year: 2026, zone: "UTC".into() }),
        meta: parking_lot::Mutex::new(Meta { tag: Tag("#one".into()) }),
        address: parking_lot::RwLock::new("127.0.0.1".parse().unwrap()),
        queue: VecDeque::from([
            Job { id: 1, done: parking_lot::Mutex::new(false) },
            Job { id: 2, done: parking_lot::Mutex::new(false) },
        ]),
        seen: HashSet::from([7]),
        shared: Rc::new(5),
        counter: tokio::sync::Mutex::new(1),
        settings: tokio::sync::RwLock::new(vec!["a".into()]),
    });
    let mut registry = Registry::new();
    registry.add("state", Rc::clone(&state));
    (registry, state)
}

fn result(registry: &Registry, command: &str) -> serde_json::Value {
    let reply = execute(registry, command);
    assert_eq!(reply["ok"], json!(true), "{command}: {reply}");
    reply["result"].clone()
}

fn code(registry: &Registry, command: &str) -> serde_json::Value {
    execute(registry, command)["error"]["code"].clone()
}

#[test]
fn serialize_gives_a_structured_view_and_deserialize_writes() {
    let (registry, state) = registry();
    assert_eq!(result(&registry, "get state.stamp"), json!({ "year": 2026, "zone": "UTC" }));
    assert_eq!(result(&registry, "get state.stamp.zone"), json!("UTC"));
    assert_eq!(result(&registry, "ls state.stamp")["type"], json!("Stamp"));

    result(&registry, r#"set state.stamp {"year": 2030, "zone": "CET"}"#);
    assert_eq!(*state.stamp.lock(), Stamp { year: 2030, zone: "CET".into() });
    result(&registry, "set state.stamp.year 2031");
    assert_eq!(state.stamp.lock().year, 2031, "a nested write round-trips through JSON");
    assert_eq!(code(&registry, r#"set state.stamp {"year": "soon"}"#), json!("type_mismatch"));
    assert_eq!(state.stamp.lock().year, 2031, "a failed write leaves the value alone");

    assert_eq!(result(&registry, "get state.address"), json!("127.0.0.1"));
    result(&registry, "set state.address 10.0.0.1");
    assert_eq!(state.address.read().to_string(), "10.0.0.1");
}

#[test]
fn from_str_writes_and_debug_reads() {
    let (registry, state) = registry();
    assert_eq!(result(&registry, "get state.meta.tag"), json!("Tag(\"#one\")"));
    result(&registry, "set state.meta.tag #two");
    assert_eq!(state.meta.lock().tag, Tag("#two".into()));
    assert_eq!(code(&registry, "set state.meta.tag three"), json!("type_mismatch"));
}

#[test]
fn iterable_collections_stay_navigable_and_writable_through_their_items() {
    let (registry, state) = registry();
    assert_eq!(result(&registry, "get state.queue 3"), json!([{ "id": 1, "done": false }, { "id": 2, "done": false }]));
    assert_eq!(result(&registry, "get state.queue[1].id"), json!(2));
    result(&registry, "set state.queue[1].done true");
    assert!(*state.queue[1].done.lock(), "an item's own lock is written through");
    assert_eq!(code(&registry, "set state.queue[1].id 9"), json!("not_writable"));
    assert_eq!(result(&registry, "get state.seen"), json!([7]));

    assert_eq!(result(&registry, "get state.bag[1].id"), json!(11));
    result(&registry, "set state.bag[0].done true");
    assert!(*state.bag.0[0].done.lock(), "an item of a third-party collection is written through its own lock");
    assert_eq!(result(&registry, "ls state.bag")["kind"], json!("sequence"));
    assert_eq!(result(&registry, "get state.index"), json!({ "first": { "id": 20, "done": false } }));
    result(&registry, "set state.index.first.done true");
    assert!(*state.index.0["first"].done.lock());
    assert_eq!(result(&registry, "get state.shared"), json!(5));
}

#[test]
fn tokio_locks_are_read_and_written_and_a_held_one_reports_busy() {
    let (registry, state) = registry();
    result(&registry, "set state.counter 42");
    assert_eq!(*state.counter.try_lock().unwrap(), 42);
    assert_eq!(result(&registry, "get state.settings"), json!(["a"]));
    let held = state.counter.try_lock().unwrap();
    assert_eq!(code(&registry, "get state.counter"), json!("lock_busy"));
    drop(held);
}

#[test]
fn third_party_values_can_be_built_for_calls() {
    let stamp: Stamp = scry::__private::probe_arg!(Stamp, &json!({ "year": 1999, "zone": "X" })).unwrap();
    assert_eq!(stamp.year, 1999);
    let tag: Tag = scry::__private::probe_arg!(Tag, &json!("#t")).unwrap();
    assert_eq!(tag, Tag("#t".into()));
    let address: IpAddr = scry::__private::probe_arg!(IpAddr, &json!("::1")).unwrap();
    assert!(address.is_loopback());
}

/// Nothing at all: shown by type name, wherever it is.
struct Secret;

/// Foreign types inside containers, as written in the source: peeled layer by layer.
#[derive(Reflect)]
struct Peeled {
    locked: parking_lot::Mutex<Tag>,
    tags: Vec<Tag>,
    maybe: Option<Tag>,
    shared: std::sync::Arc<std::sync::Mutex<Vec<Tag>>>,
    by_name: std::collections::HashMap<String, Tag>,
    boxed: Box<Tag>,
    fixed: [Option<Tag>; 2],
    secret: std::cell::RefCell<Secret>,
}

fn peeled() -> (Registry, Rc<Peeled>) {
    let tag = |text: &str| Tag(text.to_owned());
    let state = Rc::new(Peeled {
        locked: parking_lot::Mutex::new(tag("#locked")),
        tags: vec![tag("#a"), tag("#b")],
        maybe: None,
        shared: std::sync::Arc::new(std::sync::Mutex::new(vec![tag("#s")])),
        by_name: [("first".to_owned(), tag("#f"))].into(),
        boxed: Box::new(tag("#box")),
        fixed: [Some(tag("#x")), None],
        secret: std::cell::RefCell::new(Secret),
    });
    let mut registry = Registry::new();
    registry.add("peeled", Rc::clone(&state));
    (registry, state)
}

#[test]
fn foreign_types_inside_containers_are_reached_and_written() {
    let (registry, state) = peeled();
    assert_eq!(result(&registry, "get peeled.locked"), json!("Tag(\"#locked\")"));
    result(&registry, "set peeled.locked #new");
    assert_eq!(*state.locked.lock(), Tag("#new".into()), "written under the lock through FromStr");
    assert_eq!(code(&registry, "set peeled.locked nope"), json!("type_mismatch"));

    assert_eq!(result(&registry, "get peeled.tags"), json!(["Tag(\"#a\")", "Tag(\"#b\")"]));
    assert_eq!(result(&registry, "ls peeled.tags")["kind"], json!("sequence"));
    assert_eq!(code(&registry, "set peeled.tags[1] #c"), json!("not_writable"), "the registry holds it shared");

    assert_eq!(result(&registry, "get peeled.maybe"), json!(null));
    assert_eq!(result(&registry, "get peeled.shared[0]"), json!("Tag(\"#s\")"));
    result(&registry, "set peeled.shared[0] #t");
    assert_eq!(state.shared.lock().unwrap()[0], Tag("#t".into()), "through Arc, the lock and the Vec");

    assert_eq!(result(&registry, "get peeled.by_name.first"), json!("Tag(\"#f\")"));
    assert_eq!(result(&registry, "get peeled.boxed"), json!("Tag(\"#box\")"));
    assert_eq!(result(&registry, "get peeled.fixed"), json!(["Tag(\"#x\")", null]));
    assert_eq!(result(&registry, "get peeled.secret"), json!("<Secret>"));
}

#[test]
fn peeled_values_are_written_with_exclusive_access() {
    use scry::__private::probe_mut;
    let tag = |text: &str| Tag(text.to_owned());
    let mut state = Peeled {
        locked: parking_lot::Mutex::new(tag("#l")),
        tags: vec![tag("#a"), tag("#b")],
        maybe: None,
        shared: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        by_name: [("first".to_owned(), tag("#f"))].into(),
        boxed: Box::new(tag("#box")),
        fixed: [None, None],
        secret: std::cell::RefCell::new(Secret),
    };
    let set = |state: &mut Peeled, path: &str, value: serde_json::Value| {
        // `a.b[1]` as segments.
        let segments: Vec<scry::Segment> = path
            .split(['.', '['])
            .map(|part| match part.strip_suffix(']') {
                Some(index) => scry::Segment::Index(index.parse().unwrap()),
                None => scry::Segment::Field(part.to_owned()),
            })
            .collect();
        let mut result = Ok(());
        probe_mut!(state).write(&segments, &mut |target| {
            result = target.assign(&value);
            Ok(())
        })?;
        result
    };
    set(&mut state, "tags[1]", json!("#c")).unwrap();
    assert_eq!(state.tags[1], tag("#c"));
    set(&mut state, "maybe", json!("#m")).unwrap();
    assert_eq!(state.maybe, Some(tag("#m")), "assigning into None builds the value");
    set(&mut state, "maybe", json!(null)).unwrap();
    assert_eq!(state.maybe, None);
    set(&mut state, "by_name.first", json!("#g")).unwrap();
    assert_eq!(state.by_name["first"], tag("#g"));
    set(&mut state, "boxed", json!("#b2")).unwrap();
    assert_eq!(*state.boxed, tag("#b2"));
    set(&mut state, "fixed[1]", json!("#y")).unwrap();
    assert_eq!(state.fixed[1], Some(tag("#y")));
    assert_eq!(set(&mut state, "tags", json!(["#z"])).unwrap_err().code(), "not_writable");
}
