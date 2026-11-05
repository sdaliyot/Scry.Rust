//! Calling functions and methods through shims shaped like the ones `cargo scry` emits.

// The probes' autoref specialization needs exactly these borrows.
#![allow(clippy::needless_borrow)]

use std::sync::{Arc, Mutex};

use scry::__private::{Function, Receiver, This};
#[allow(unused_imports)] // which one resolves depends on the type
use scry::__private::{ViaClone as _, ViaNoClone as _};
use scry::{Reflect, Registry, Value, execute};
use serde_json::json;

#[derive(Reflect, Clone, Debug, PartialEq)]
enum Shape {
    Dot,
    Circle { radius: u32 },
    Pair(u8, u8),
}

#[derive(Reflect, Debug, PartialEq)]
struct Sprite {
    name: String,
    shape: Shape,
    tags: Vec<String>,
    hidden: Option<bool>,
}

#[derive(Reflect)]
struct Scene {
    sprites: Mutex<Vec<Sprite>>,
    frozen: Vec<Sprite>,
}

impl Sprite {
    fn label(&self, prefix: &str, times: u32) -> String {
        format!("{prefix}{}", self.name.repeat(times as usize))
    }

    fn rename(&mut self, name: String) -> usize {
        self.name = name;
        self.name.len()
    }
}

impl Shape {
    fn area_hint(self) -> u32 {
        match self {
            Shape::Dot => 0,
            Shape::Circle { radius } => 3 * radius * radius,
            Shape::Pair(a, b) => u32::from(a) * u32::from(b),
        }
    }
}

fn build(sprite: Sprite) -> Sprite {
    sprite
}

fn explode(_reason: &str) -> u8 {
    panic!("boom")
}

fn total(numbers: &[i64]) -> i64 {
    numbers.iter().sum()
}

/// The shape of a generated shim, minus the attributes.
macro_rules! shim {
    ($slot:ident, $name:literal, $path:literal, $receiver:ident, $self_ty:ty, $arity:literal, $invoke:expr) => {
        #[scry::__private::linkme::distributed_slice(scry::__private::FUNCTIONS)]
        #[linkme(crate = scry::__private::linkme)]
        static $slot: Function = {
            fn self_type() -> std::any::TypeId {
                std::any::TypeId::of::<$self_ty>()
            }
            Function {
                name: $name,
                path: $path,
                signature: $path,
                receiver: Receiver::$receiver,
                self_type: if matches!(Receiver::$receiver, Receiver::None) { None } else { Some(self_type) },
                trait_name: None,
                arity: $arity,
                invoke: $invoke,
            }
        };
    };
}

fn result(value: &impl Reflect, depth: usize) -> Result<Value, scry::Error> {
    Ok(value.snapshot(depth))
}

shim!(LABEL, "label", "calls::Sprite::label", Ref, Sprite, 2, |this: This<'_>, args: &[Value], depth| {
    let this = this.downcast_ref::<Sprite>()?;
    let arg0 = scry::__private::probe_ref_arg!(str, &args[0])?;
    let arg1: u32 = scry::__private::probe_arg!(u32, &args[1])?;
    result(&Sprite::label(this, std::borrow::Borrow::<str>::borrow(&arg0), arg1), depth)
});

shim!(RENAME, "rename", "calls::Sprite::rename", Mut, Sprite, 1, |this: This<'_>, args: &[Value], depth| {
    let this = this.downcast_mut::<Sprite>()?;
    let arg0: String = scry::__private::probe_arg!(String, &args[0])?;
    result(&Sprite::rename(this, arg0), depth)
});

shim!(AREA, "area_hint", "calls::Shape::area_hint", Ref, Shape, 0, |this: This<'_>, _args: &[Value], depth| {
    let this = this.downcast_ref::<Shape>()?;
    let this: Shape = (&&&scry::__private::SelfProbe(this)).scry_self()?;
    result(&Shape::area_hint(this), depth)
});

shim!(BUILD, "build", "calls::build", None, (), 1, |_this: This<'_>, args: &[Value], depth| {
    let arg0: Sprite = scry::__private::probe_arg!(Sprite, &args[0])?;
    let result = build(arg0);
    Ok(scry::__private::probe_ref!(&result).snapshot(depth))
});

shim!(EXPLODE, "explode", "calls::explode", None, (), 1, |_this: This<'_>, args: &[Value], depth| {
    let arg0 = scry::__private::probe_ref_arg!(str, &args[0])?;
    result(&explode(std::borrow::Borrow::<str>::borrow(&arg0)), depth)
});

shim!(TOTAL, "total", "calls::total", None, (), 1, |_this: This<'_>, args: &[Value], depth| {
    let arg0 = scry::__private::probe_ref_arg!([i64], &args[0])?;
    result(&total(std::borrow::Borrow::<[i64]>::borrow(&arg0)), depth)
});

shim!(OPAQUE, "opaque", "calls::opaque", None, (), 1, |_this: This<'_>, args: &[Value], depth| {
    let arg0: std::time::Instant = scry::__private::probe_arg!(std::time::Instant, &args[0])?;
    result(&format!("{arg0:?}"), depth)
});

fn sprite(name: &str) -> Sprite {
    Sprite { name: name.into(), shape: Shape::Circle { radius: 2 }, tags: vec![], hidden: None }
}

fn registry() -> (Registry, Arc<Scene>) {
    let scene =
        Arc::new(Scene { sprites: Mutex::new(vec![sprite("ant"), sprite("bee")]), frozen: vec![sprite("cat")] });
    let mut registry = Registry::new();
    registry.add("scene", Arc::clone(&scene));
    (registry, scene)
}

fn code(result: Result<Value, scry::Error>) -> &'static str {
    result.expect_err("expected an error").code()
}

#[test]
fn methods_run_on_live_values() {
    let (registry, scene) = registry();
    assert_eq!(registry.call("scene.sprites[1].label", &[json!("> "), json!(2)], 3).unwrap(), json!("> beebee"));
    assert_eq!(registry.call("scene.sprites[0].rename", &[json!("wasp")], 3).unwrap(), json!(4));
    assert_eq!(scene.sprites.lock().unwrap()[0].name, "wasp");
    assert_eq!(registry.call("scene.sprites[0].shape.area_hint", &[], 3).unwrap(), json!(12));
}

#[test]
fn a_mut_method_needs_interior_mutability_like_a_write() {
    let (registry, scene) = registry();
    assert_eq!(registry.call("scene.frozen[0].label", &[json!(""), json!(1)], 3).unwrap(), json!("cat"));
    assert_eq!(code(registry.call("scene.frozen[0].rename", &[json!("dog")], 3)), "not_writable");
    assert_eq!(scene.frozen[0].name, "cat");
}

#[test]
fn arguments_are_built_from_json() {
    let (registry, _) = registry();
    let built = json!({ "name": "eel", "shape": { "Pair": [2, 3] }, "tags": ["x"], "hidden": true });
    let snapshot = registry.call("build", &[built], 3).unwrap();
    assert_eq!(snapshot["shape"], json!({ "Pair": { "0": 2, "1": 3 } }));
    assert_eq!(
        registry.call("build", std::slice::from_ref(&snapshot), 3).unwrap(),
        snapshot,
        "a snapshot builds the same value"
    );
    let sparse = registry.call("calls::build", &[json!({ "name": "fly", "shape": "Dot", "tags": [] })], 3).unwrap();
    assert_eq!(sparse["hidden"], json!(null));
    assert_eq!(registry.call("total", &[json!([1, 2, 3])], 3).unwrap(), json!(6));

    assert_eq!(
        code(registry.call("build", &[json!({ "name": "x", "shape": "Square", "tags": [] })], 3)),
        "type_mismatch"
    );
    assert_eq!(
        code(registry.call("build", &[json!({ "name": "x", "shape": "Dot", "tags": [], "extra": 1 })], 3)),
        "no_such_member"
    );
    assert_eq!(code(registry.call("scene.sprites[0].label", &[json!("x"), json!(-1)], 3)), "type_mismatch");
    assert_eq!(code(registry.call("opaque", &[json!(1)], 3)), "cannot_convert");
}

#[test]
fn bad_calls_are_reported() {
    let (registry, scene) = registry();
    assert_eq!(code(registry.call("nope", &[], 3)), "no_such_function");
    assert_eq!(code(registry.call("scene.sprites[0].nope", &[], 3)), "no_such_method");
    assert_eq!(code(registry.call("scene.sprites[0].label", &[], 3)), "argument_count");
    assert_eq!(code(registry.call("explode", &[json!("why")], 3)), "panicked");
    assert_eq!(code(registry.call("scene.sprites", &[], 3)), "no_such_method");
    assert!(!scene.sprites.is_poisoned());
}

#[test]
fn commands_list_and_call() {
    let (registry, _) = registry();
    assert_eq!(execute(&registry, "call scene.sprites[1].label \"- \" 1")["result"], json!("- bee"));
    assert_eq!(execute(&registry, "call total [4, 5]")["result"], json!(9));
    let methods = execute(&registry, "ls scene.sprites[0]")["result"]["methods"].clone();
    assert_eq!(methods, json!(["calls::Sprite::label", "calls::Sprite::rename"]));
    let listed = execute(&registry, "fns calls::sprite")["result"].as_array().unwrap().len();
    assert_eq!(listed, 2);
}

#[cfg(feature = "script")]
#[test]
fn scripts_call_with_method_syntax() {
    let (registry, scene) = registry();
    let reply = execute(
        &registry,
        "eval let s = scene.sprites[1]; s.rename(\"moth\"); #{ label: s.label(\"\", 2), total: total([1, 1]), \
         area: s.shape.area_hint(), via: invoke(\"calls::build\", [s]).name }",
    );
    assert_eq!(reply["result"], json!({ "label": "mothmoth", "total": 2, "area": 12, "via": "moth" }), "{reply}");
    assert_eq!(scene.sprites.lock().unwrap()[1].name, "moth");
}
