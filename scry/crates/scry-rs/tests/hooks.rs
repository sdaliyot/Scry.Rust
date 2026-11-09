//! Hooks through hook points shaped like the ones `cargo scry` compiles into each function.

// The probes' autoref specialization needs exactly these borrows.
#![allow(clippy::needless_borrow)]

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use scry::__private::HookSite;
use scry::{Reflect, Registry, execute};
use serde_json::{Value, json};

/// A function with a hook point, written the way the generator writes one. Every parameter is
/// offered for changing here; the generator leaves out `&T` ones.
macro_rules! hooked {
    (fn $name:ident($($arg:ident: $ty:ty),*) -> $ret:ty $body:block) => {
        #[allow(unused_mut)]
        fn $name($($arg: $ty),*) -> $ret {
            static SITE: HookSite = HookSite::new(concat!("hooks::", stringify!($name)), stringify!(fn $name));
            #[scry::__private::linkme::distributed_slice(scry::__private::HOOK_SITES)]
            #[linkme(crate = scry::__private::linkme)]
            static REGISTERED: &HookSite = &SITE;
            let mut call = SITE.enter(|capture| {
                $(capture.arg(stringify!($arg), scry::__private::probe_ref!(&$arg));)*
            });
            $(let mut $arg = $arg;)*
            let skip: Option<$ret> = match &mut call {
                Some(c) => match c.before() {
                    Some(changes) => {
                        $(if let Some(value) = changes.arg(stringify!($arg)) {
                            c.change(stringify!($arg), scry::__private::probe_mut!(&mut $arg), value);
                        })*
                        match changes.result() {
                            Some(value) => c.skip(value, scry::__private::probe_arg!($ret, value)),
                            None => None,
                        }
                    }
                    None => None,
                },
                None => None,
            };
            let mut result: $ret = match skip {
                Some(skipped) => skipped,
                None => scry::__private::run_once(move || -> $ret { $body }),
            };
            if let Some(c) = &mut call {
                if let Some(value) = c.after(scry::__private::probe_ref!(&result)) {
                    match c.replace(&value, scry::__private::probe_arg!($ret, &value)) {
                        Some(replacement) => result = replacement,
                        None => c.change("result", scry::__private::probe_mut!(&mut result), &value),
                    }
                }
            }
            if let Some(call) = call {
                call.exit(scry::__private::probe_ref!(&result));
            }
            result
        }
    };
}

hooked!(
    fn repeat(times: usize, word: &str) -> Option<String> {
        if times > 5 {
            return None;
        }
        Some(word.repeat(times))
    }
);

hooked!(
    fn explode(reason: &str) -> u8 {
        panic!("exploded: {reason}")
    }
);

hooked!(
    fn tick(n: u64) -> u64 {
        n + 1
    }
);

hooked!(
    fn locked(value: &Mutex<u8>) -> u8 {
        0
    }
);

hooked!(
    fn quiet(n: u8) -> u8 {
        n
    }
);

#[derive(Reflect)]
struct Order {
    id: u32,
    lines: Vec<String>,
}

hooked!(
    fn place(order: &Order) -> bool {
        !order.lines.is_empty()
    }
);

fn run(command: &str) -> Value {
    let reply = execute(&Registry::new(), command);
    assert_eq!(reply["ok"], json!(true), "{command}: {reply}");
    reply["result"].clone()
}

fn add(path: &str, options: &str) -> u64 {
    run(&format!("hook add {path} {options}"))["hook"].as_u64().unwrap()
}

#[test]
fn records_arguments_and_results_while_on() {
    repeat(1, "off");
    let id = add("hooks::repeat", "");
    assert_eq!(repeat(2, "ab"), Some("abab".into()));
    assert_eq!(repeat(9, "x"), None);
    let page = run(&format!("hook read {id}"));
    let calls = page["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 2, "the call before `hook add` is not recorded: {page}");
    assert_eq!(calls[0]["args"], json!({ "times": 2, "word": "ab" }));
    assert_eq!(calls[0]["result"], json!("abab"));
    assert_eq!(calls[1]["result"], json!(null));
    assert_eq!(calls[1]["seq"], json!(1));
    assert_eq!(page["cursor"], json!(2));
    assert_eq!(run(&format!("hook read {id} 2"))["calls"], json!([]));

    run(&format!("hook remove {id}"));
    repeat(3, "c");
    assert_eq!(execute(&Registry::new(), &format!("hook read {id}"))["ok"], json!(false), "removed");
}

#[test]
fn records_a_panic_and_lets_it_continue() {
    let id = add("explode", "");
    let unwound = std::panic::catch_unwind(|| explode("on purpose"));
    assert!(unwound.is_err(), "the hook must not swallow the panic");
    let calls = run(&format!("hook read {id}"))["calls"].clone();
    let panic = calls[0]["panic"].as_str().unwrap();
    assert!(panic.contains("exploded: on purpose") && panic.contains("hooks.rs"), "{panic}");
    assert_eq!(calls[0]["args"]["reason"], json!("on purpose"));
}

#[test]
fn waits_for_a_matching_call_from_another_thread() {
    let id = add("tick", "");
    let worker = thread::spawn(|| {
        for n in 0..50 {
            tick(n);
            thread::sleep(Duration::from_millis(5));
        }
    });
    let started = Instant::now();
    let waited = run(&format!("hook wait {id} timeout=10 args.n == 20 && result == 21"));
    assert_eq!(waited["satisfied"], json!(true), "{waited}");
    assert_eq!(waited["call"]["args"]["n"], json!(20));
    assert!(waited["call"]["thread"].as_str().unwrap().starts_with("thread"), "{waited}");
    assert!(started.elapsed() < Duration::from_secs(5));
    worker.join().unwrap();

    let timed_out = run(&format!("hook wait {id} timeout=1 args.n == 1000"));
    assert_eq!(timed_out["satisfied"], json!(false));
    let bad = execute(&Registry::new(), &format!("hook wait {id} timeout=1 args.n +"));
    assert_eq!(bad["error"]["code"], json!("script_error"));
}

#[test]
fn a_full_buffer_drops_the_oldest_calls() {
    let id = add("quiet", "capacity=3");
    for n in 0..5 {
        quiet(n);
    }
    let page = run(&format!("hook read {id}"));
    assert_eq!(page["droppedCalls"], json!(2));
    assert_eq!(page["truncated"], json!(true));
    assert_eq!(page["calls"][0]["args"]["n"], json!(2));
}

#[test]
fn a_capture_does_not_wait_for_a_lock_the_thread_holds() {
    let id = add("locked", "");
    let value = Mutex::new(7u8);
    let guard = value.lock().unwrap();
    let started = Instant::now();
    locked(&value);
    assert!(started.elapsed() < Duration::from_millis(500), "the capture waited for the lock");
    drop(guard);
    let call = run(&format!("hook read {id}"))["calls"][0].clone();
    assert_eq!(call["args"]["value"], json!("<locked: Mutex<u8>>"), "{call}");
    locked(&value);
    assert_eq!(run(&format!("hook read {id} 1"))["calls"][0]["args"]["value"], json!(7));
}

#[test]
fn listing_adding_twice_and_errors() {
    let listed = run("hooks hooks::place");
    assert_eq!(listed, json!([{ "path": "hooks::place", "signature": "fn place", "hook": null }]));
    let id = add("place", "depth=1");
    assert_eq!(run("hooks hooks::place")[0]["hook"], json!(id));
    assert_eq!(run("hook add place")["existing"], json!(true));
    place(&Order { id: 1, lines: vec!["a".into()] });
    let order = run(&format!("hook read {id}"))["calls"][0]["args"]["order"].clone();
    assert_eq!(order, json!({ "id": 1, "lines": "<Vec<String>>" }), "depth=1 bounds the snapshot");

    let error = |command: &str| execute(&Registry::new(), command)["error"]["code"].clone();
    assert_eq!(error("hook add nope"), json!("no_such_function"));
    assert_eq!(error("hook add place depth=x"), json!("bad_command"));
    assert_eq!(error("hook read 999999"), json!("bad_command"));
    let _ = Arc::new(());
}

#[cfg(feature = "script")]
hooked!(
    fn scripted(n: u8) -> u8 {
        n * 2
    }
);

#[cfg(feature = "script")]
#[test]
fn scripts_add_and_read_hooks() {
    let id = run(r#"eval hook_add("scripted", "depth=1").hook"#).as_u64().unwrap();
    scripted(4);
    let read = run(&format!("eval let page = hook_read({id}); page.calls[0].result"));
    assert_eq!(read, json!(8));
    assert_eq!(run(&format!("eval hook_remove({id}).calls")), json!(1));
}

#[derive(Reflect, Debug, PartialEq)]
struct Quote {
    price: u32,
    note: String,
}

hooked!(
    fn price(base: u32, quote: Quote) -> Result<Quote, String> {
        if base == 0 {
            return Err("free".into());
        }
        Ok(Quote { price: base + quote.price, note: quote.note })
    }
);

hooked!(
    fn stamp(count: u32) -> Quote {
        Quote { price: count, note: "real".into() }
    }
);

hooked!(
    fn discount(base: u32, quote: Quote) -> Result<Quote, String> {
        Ok(Quote { price: base + quote.price, note: quote.note })
    }
);

hooked!(
    fn tally(count: u32) -> Quote {
        Quote { price: count, note: "real".into() }
    }
);

#[test]
fn a_before_script_changes_arguments() {
    let id = add("hooks::price", "");
    run(&format!(r#"hook before {id} #{{ args: #{{ base: args.base * 10, quote: #{{ note: "changed" }} }} }}"#));
    let quote = price(2, Quote { price: 1, note: "kept?".into() }).unwrap();
    assert_eq!(quote, Quote { price: 21, note: "changed".into() });
    let call = run(&format!("hook read {id}"))["calls"][0].clone();
    assert_eq!(call["args"]["base"], json!(2), "the record keeps the arguments as they came in: {call}");
    assert_eq!(call["changes"]["base"], json!(20));
    assert_eq!(run("hooks hooks::price")[0]["intervenes"], json!(true));

    run(&format!("hook before {id}"));
    assert_eq!(price(2, Quote { price: 1, note: "n".into() }).unwrap().price, 3, "an empty script removes it");
}

#[test]
fn a_before_script_can_skip_the_body() {
    let id = add("hooks::stamp", "");
    run(&format!(r#"hook before {id} if args.count > 5 {{ #{{ result: #{{ price: 0, note: "skipped" }} }} }}"#));
    assert_eq!(stamp(9), Quote { price: 0, note: "skipped".into() });
    assert_eq!(stamp(1), Quote { price: 1, note: "real".into() }, "the script returned () for this call");
    let calls = run(&format!("hook read {id}"))["calls"].clone();
    assert_eq!(calls[0]["skipped"], json!(true));
    assert_eq!(calls[0]["result"]["note"], json!("skipped"));
    assert_eq!(calls[1]["skipped"], json!(null));
}

#[test]
fn an_after_script_replaces_the_result() {
    let id = add("hooks::discount", "");
    run(&format!(r#"hook after {id} if args.base == 7 {{ #{{ result: #{{ Err: "injected" }} }} }}"#));
    assert_eq!(discount(7, Quote { price: 1, note: "n".into() }), Err("injected".into()));
    assert!(discount(8, Quote { price: 1, note: "n".into() }).is_ok());
    run(&format!("hook remove {id}"));
}

#[test]
fn a_failing_change_leaves_the_call_alone_and_is_recorded() {
    let id = add("hooks::tally", "");
    run(&format!(r#"hook before {id} #{{ args: #{{ count: "not a number" }} }}"#));
    assert_eq!(tally(3).price, 3);
    run(&format!(r#"hook before {id} throw "broken""#));
    assert_eq!(tally(4).price, 4);
    let page = run(&format!("hook read {id}"));
    assert!(page["calls"][0]["changeErrors"][0].as_str().unwrap().starts_with("count:"), "{page}");
    assert!(page["calls"][1]["changeErrors"][0].as_str().unwrap().contains("broken"), "{page}");
    assert_eq!(page["changeFailures"], json!(2));
    let bad = execute(&Registry::new(), &format!("hook before {id} let x = ;"));
    assert_eq!(bad["error"]["code"], json!("script_error"), "a script that does not compile is refused up front");
    run(&format!("hook remove {id}"));
}

/// One function whose body an attribute macro compiled twice - into a handler and a plain
/// function, say - so it has two hook sites with the same path.
macro_rules! copy_of_body {
    ($name:ident) => {
        fn $name(n: u64) -> u64 {
            static SITE: HookSite = HookSite::new("hooks::handler", "fn handler(n: u64) -> u64");
            #[scry::__private::linkme::distributed_slice(scry::__private::HOOK_SITES)]
            #[linkme(crate = scry::__private::linkme)]
            static REGISTERED: &HookSite = &SITE;
            let call = SITE.enter(|capture| capture.arg("n", scry::__private::probe_ref!(&n)));
            let result = n * 2;
            if let Some(call) = call {
                call.exit(scry::__private::probe_ref!(&result));
            }
            result
        }
    };
}

copy_of_body!(handler_as_route);
copy_of_body!(handler_as_function);

#[test]
fn a_hook_sees_every_copy_of_a_body() {
    let listed = run("hooks hooks::handler");
    assert_eq!(listed.as_array().unwrap().len(), 1, "one function, listed once: {listed}");
    let id = add("handler", "");
    handler_as_route(1);
    handler_as_function(2);
    let calls = run(&format!("hook read {id}"))["calls"].clone();
    let args: Vec<&Value> = calls.as_array().unwrap().iter().map(|call| &call["args"]["n"]).collect();
    assert_eq!(args, [&json!(1), &json!(2)]);
    run(&format!("hook remove {id}"));
    handler_as_route(3);
    assert!(run("hooks hooks::handler")[0]["hook"].is_null());
}
