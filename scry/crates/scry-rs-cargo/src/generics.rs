//! Generic functions and types: which concrete instantiations the crate itself uses, and how a
//! shim spells their type arguments.
//!
//! The instantiations come from rustc's MIR (`--emit=mir`, a stable flag), which prints every call
//! with its generic arguments filled in: `clamp_all::<f64>(..)`, `Cache::<u32, String>::get(..)`,
//! `<Wrapper<u8> as Describe>::describe(..)`. Only calls count - a call proves the type arguments
//! meet the function's or the impl block's bounds, so a shim for them compiles.
//!
//! MIR shortens a path to its last segment when that name is unique (`Item`, `String`), so each
//! argument is resolved again: against the crate's own types, with their visibility checked from
//! the shim's module, and against a table of common std types. An argument that cannot be named
//! from there - a private type of another module, a closure, a type parameter of the caller -
//! drops that instantiation.

use std::collections::{HashMap, HashSet};

use syn::visit_mut::{self, VisitMut};
use syn::{Item, Type, Visibility};

/// Std types MIR may print by their last segment, and where they live.
const STD_TYPES: &[(&str, &str)] = &[
    ("Arc", "::std::sync::Arc"),
    ("Rc", "::std::rc::Rc"),
    ("Weak", "::std::sync::Weak"),
    ("Mutex", "::std::sync::Mutex"),
    ("RwLock", "::std::sync::RwLock"),
    ("Cell", "::std::cell::Cell"),
    ("RefCell", "::std::cell::RefCell"),
    ("HashMap", "::std::collections::HashMap"),
    ("HashSet", "::std::collections::HashSet"),
    ("BTreeMap", "::std::collections::BTreeMap"),
    ("BTreeSet", "::std::collections::BTreeSet"),
    ("VecDeque", "::std::collections::VecDeque"),
    ("Duration", "::std::time::Duration"),
    ("Instant", "::std::time::Instant"),
    ("SystemTime", "::std::time::SystemTime"),
    ("PathBuf", "::std::path::PathBuf"),
    ("Path", "::std::path::Path"),
    ("OsString", "::std::ffi::OsString"),
    ("OsStr", "::std::ffi::OsStr"),
    ("Cow", "::std::borrow::Cow"),
];

/// Names that are in scope everywhere: primitives and the prelude.
const IN_SCOPE: &[&str] = &[
    "bool",
    "char",
    "str",
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "usize",
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "isize",
    "f32",
    "f64",
    "String",
    "Vec",
    "Box",
    "Option",
    "Result",
    "Send",
    "Sync",
    "Sized",
    "Unpin",
    "Copy",
    "Clone",
    "Fn",
    "FnMut",
    "FnOnce",
    "Iterator",
    "ToString",
    "Default",
    "Debug",
    "PartialEq",
    "Eq",
    "PartialOrd",
    "Ord",
];

/// One concrete use of a generic function or type, as MIR printed it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Use {
    /// The path segments before the name (`["util"]` in `util::clamp_all::<f64>`).
    pub qualifier: Vec<String>,
    pub args: Vec<String>,
    /// The method called on a generic type; `None` for a generic function.
    pub method: Option<String>,
}

/// The crate's own types and modules, with where they are and who can see them.
#[derive(Default)]
pub struct Index {
    /// Type and trait names, to the modules defining them and whether they are `pub`.
    items: HashMap<String, Vec<(Vec<String>, bool)>>,
    /// Module paths, to whether they are `pub`.
    modules: HashMap<Vec<String>, bool>,
    /// Names of generic functions and types, to scan MIR for.
    pub generic_names: HashSet<String>,
}

impl Index {
    /// Adds a file's items; `module` is the file's module path from the crate root.
    pub fn add_file(&mut self, module: &[String], file: &syn::File) {
        self.add_items(module, &file.items);
    }

    fn add_items(&mut self, module: &[String], items: &[Item]) {
        for item in items {
            let (name, vis, generic) = match item {
                Item::Struct(item) => (&item.ident, &item.vis, has_type_params(&item.generics)),
                Item::Enum(item) => (&item.ident, &item.vis, has_type_params(&item.generics)),
                Item::Union(item) => (&item.ident, &item.vis, false),
                Item::Type(item) => (&item.ident, &item.vis, false),
                Item::Trait(item) => (&item.ident, &item.vis, false),
                Item::Fn(item) => {
                    if has_type_params(&item.sig.generics) {
                        self.generic_names.insert(item.sig.ident.to_string());
                    }
                    continue;
                }
                Item::Mod(item) => {
                    let mut path = module.to_vec();
                    path.push(item.ident.to_string());
                    self.modules.insert(path.clone(), is_pub(&item.vis));
                    if let Some((_, items)) = &item.content {
                        self.add_items(&path, items);
                    }
                    continue;
                }
                _ => continue,
            };
            if generic {
                self.generic_names.insert(name.to_string());
            }
            self.items.entry(name.to_string()).or_default().push((module.to_vec(), is_pub(vis)));
        }
    }

    /// The module defining the type `name` as seen from `from`: the only one, or `from` itself.
    pub fn module_of(&self, name: &str, from: &[String]) -> Option<Vec<String>> {
        let defs = self.items.get(name)?;
        match defs.as_slice() {
            [(module, _)] => Some(module.clone()),
            _ => defs.iter().find(|(module, _)| module == from).map(|(module, _)| module.clone()),
        }
    }

    /// Whether `module::name` can be named from `from`: every private module on the way, and a
    /// private item itself, must be `from` or one of its ancestors.
    fn visible(&self, module: &[String], public: bool, from: &[String]) -> bool {
        let reachable = |parent: &[String], public: bool| public || from.starts_with(parent);
        (1..=module.len())
            .all(|depth| reachable(&module[..depth - 1], self.modules.get(&module[..depth]).copied().unwrap_or(false)))
            && reachable(module, public)
    }

    /// `text`, a type as MIR prints it, rewritten to be valid in module `from`, or why it cannot be.
    pub fn resolve(&self, text: &str, from: &[String], crate_name: &str) -> Result<Type, String> {
        if text.contains('{') || text.contains("impl ") {
            return Err(format!("`{text}` cannot be named"));
        }
        let mut ty: Type = syn::parse_str(text).map_err(|_| format!("`{text}` cannot be parsed"))?;
        let mut resolver = Resolver { index: self, from, crate_name, problem: None };
        resolver.visit_type_mut(&mut ty);
        match resolver.problem {
            Some(problem) => Err(problem),
            None => Ok(ty),
        }
    }
}

fn has_type_params(generics: &syn::Generics) -> bool {
    generics.type_params().next().is_some()
}

/// `pub` and `pub(crate)` reach the whole crate; anything narrower is treated as private.
fn is_pub(vis: &Visibility) -> bool {
    match vis {
        Visibility::Public(_) => true,
        Visibility::Restricted(restricted) => restricted.path.is_ident("crate"),
        Visibility::Inherited => false,
    }
}

struct Resolver<'a> {
    index: &'a Index,
    from: &'a [String],
    crate_name: &'a str,
    problem: Option<String>,
}

impl VisitMut for Resolver<'_> {
    fn visit_type_path_mut(&mut self, ty: &mut syn::TypePath) {
        if ty.qself.is_none() {
            self.resolve(&mut ty.path);
        }
        visit_mut::visit_type_path_mut(self, ty);
    }

    fn visit_trait_bound_mut(&mut self, bound: &mut syn::TraitBound) {
        self.resolve(&mut bound.path);
        visit_mut::visit_trait_bound_mut(self, bound);
    }
}

impl Resolver<'_> {
    fn resolve(&mut self, path: &mut syn::Path) {
        let names: Vec<String> = path.segments.iter().map(|segment| segment.ident.to_string()).collect();
        let first = names[0].as_str();
        let replacement = if path.leading_colon.is_some() || IN_SCOPE.contains(&first) && names.len() == 1 {
            return;
        } else if matches!(first, "std" | "core" | "alloc") {
            let root = if first == "core" { "::core" } else { "::std" };
            format!("{root}{}", names[1..].iter().map(|name| format!("::{name}")).collect::<String>())
        } else if names.len() == 1 && self.index.items.contains_key(first) {
            let Some((module, _)) =
                self.index.items[first].iter().find(|(module, public)| self.index.visible(module, *public, self.from))
            else {
                self.problem = Some(format!("`{first}` is not visible from the shim's module"));
                return;
            };
            format!("crate{}::{first}", module.iter().map(|name| format!("::{name}")).collect::<String>())
        } else if names.len() == 1
            && let Some((_, std)) = STD_TYPES.iter().find(|(name, _)| *name == first)
        {
            std.to_string()
        } else if names.len() > 1 {
            // A path MIR did not shorten: crate-relative, maybe with the crate's own name first.
            let start = usize::from(first == self.crate_name || first == "crate");
            let (modules, name) = (&names[start..names.len() - 1], &names[names.len() - 1]);
            let defined = self.index.items.get(name).and_then(|defs| defs.iter().find(|(module, _)| module == modules));
            match defined {
                Some((module, public)) if self.index.visible(module, *public, self.from) => {
                    format!("crate{}::{name}", module.iter().map(|name| format!("::{name}")).collect::<String>())
                }
                _ => {
                    self.problem = Some(format!("`{}` cannot be named from the shim's module", names.join("::")));
                    return;
                }
            }
        } else {
            self.problem = Some(format!("`{first}` is not a type the generator knows"));
            return;
        };
        // Keep the generic arguments of the last segment.
        let arguments = path.segments.last().map(|segment| segment.arguments.clone()).unwrap_or_default();
        match syn::parse_str::<syn::Path>(&replacement) {
            Ok(mut new) => {
                if let Some(last) = new.segments.last_mut() {
                    last.arguments = arguments;
                }
                *path = new;
            }
            Err(_) => self.problem = Some(format!("`{replacement}` is not a path")),
        }
    }
}

/// Every use of the generic functions and types named in `names` in a MIR dump.
pub fn scan(mir: &str, names: &HashSet<String>) -> HashMap<String, HashSet<Use>> {
    let mut uses: HashMap<String, HashSet<Use>> = HashMap::new();
    let bytes = mir.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut index = 0;
    while index < bytes.len() {
        if !ident(bytes[index]) || (index > 0 && ident(bytes[index - 1])) {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && ident(bytes[index]) {
            index += 1;
        }
        let name = &mir[start..index];
        if !names.contains(name) {
            continue;
        }
        let rest = &mir[index..];
        let open = if rest.starts_with("::<") {
            index + 3
        } else if rest.starts_with('<') {
            index + 1
        } else {
            continue;
        };
        let Some((args, close)) = generic_args(mir, open) else { continue };
        let qualifier = qualifier(&mir[..start]);
        let after = &mir[close..];
        let method = if let Some(method) = after.strip_prefix("::") {
            Some(leading_ident(method))
        } else if after.starts_with(" as ") {
            // `<Type<args> as Trait>::method`: skip to the `>` closing the qualified path.
            trait_method(after)
        } else {
            None
        };
        let method = method.filter(|method| !method.is_empty());
        uses.entry(name.to_owned()).or_default().insert(Use { qualifier, args, method });
        index = close;
    }
    uses
}

/// The comma-separated arguments starting at `open` (just after `<`), and the position after the
/// closing `>`.
fn generic_args(text: &str, open: usize) -> Option<(Vec<String>, usize)> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut args = Vec::new();
    let mut start = open;
    let mut index = open;
    while index < bytes.len() {
        match bytes[index] {
            b'-' if bytes.get(index + 1) == Some(&b'>') => index += 1,
            b'<' | b'(' | b'[' => depth += 1,
            b'>' | b')' | b']' if depth > 0 => depth -= 1,
            b'>' => {
                args.push(text[start..index].trim().to_owned());
                return Some((args, index + 1));
            }
            b',' if depth == 0 => {
                args.push(text[start..index].trim().to_owned());
                start = index + 1;
            }
            b'\n' | b';' | b'{' => return None,
            _ => {}
        }
        index += 1;
    }
    None
}

/// The `a::b::` segments directly before a name.
fn qualifier(before: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut rest = before;
    while let Some(head) = rest.strip_suffix("::") {
        let start = head.rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).map_or(0, |index| index + 1);
        if start == head.len() {
            break;
        }
        segments.insert(0, head[start..].to_owned());
        rest = &head[..start];
    }
    segments
}

fn leading_ident(text: &str) -> String {
    text.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect()
}

fn trait_method(after: &str) -> Option<String> {
    let mut depth = 0usize;
    for (index, c) in after.char_indices() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            '>' => return after[index + 1..].strip_prefix("::").map(leading_ident),
            '\n' => return None,
            _ => {}
        }
    }
    None
}

/// Whether a use qualified by `qualifier` (crate-relative, maybe led by the crate's name) can
/// refer to an item in `module`.
pub fn qualifies(qualifier: &[String], module: &[String], crate_name: &str) -> bool {
    let qualifier = match qualifier.first().map(String::as_str) {
        Some(first) if first == crate_name || first == "crate" => &qualifier[1..],
        _ => qualifier,
    };
    module.ends_with(qualifier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_calls_with_their_type_arguments() {
        let mir = "\
            _66 = clamp_all::<i64>(move _67, const 1_i64) -> [return: bb43];\n\
            _73 = Cache::<u32, String>::get(copy _74, copy _76) -> [return: bb47];\n\
            _2 = util::describe::<Item>(copy _8) -> [return: bb1];\n\
            _4 = <Wrap<Vec<u8>> as Describe>::describe(copy _5) -> [return: bb2];\n\
            _0 = Cache::<K, V> { entries: move _2 };\n\
            _9 = apply::<fn(u8) -> u8>(copy _1) -> [return: bb3];\n";
        let names: HashSet<String> = ["clamp_all", "Cache", "describe", "Wrap", "apply"].map(String::from).into();
        let uses = scan(mir, &names);
        let one = |name: &str| uses[name].iter().cloned().collect::<Vec<_>>();
        assert_eq!(one("clamp_all"), [Use { qualifier: vec![], args: vec!["i64".into()], method: None }]);
        let cache = uses["Cache"].iter().find(|use_| use_.method.is_some()).unwrap();
        assert_eq!(
            (cache.args.clone(), cache.method.clone()),
            (vec!["u32".into(), "String".into()], Some("get".into()))
        );
        assert_eq!(one("describe")[0].qualifier, ["util"]);
        assert_eq!(
            one("Wrap")[0],
            Use { qualifier: vec![], args: vec!["Vec<u8>".into()], method: Some("describe".into()) }
        );
        assert_eq!(one("apply")[0].args, ["fn(u8) -> u8"]);
    }

    #[test]
    fn resolves_shortened_names_from_the_shims_module() {
        let file: syn::File = syn::parse_str(
            "pub struct Item; struct Hidden; mod private { pub struct Inner; } pub mod open { pub struct Wide; struct Narrow; }",
        )
        .unwrap();
        let mut index = Index::default();
        index.add_file(&["inv".to_owned()], &file);
        let from_util = ["util".to_owned()];
        let from_inv = ["inv".to_owned()];
        let spelled = |text: &str, from: &[String]| {
            index
                .resolve(text, from, "app")
                .map(|ty| quote::ToTokens::to_token_stream(&ty).to_string().replace(' ', ""))
        };
        assert_eq!(spelled("Item", &from_util).unwrap(), "crate::inv::Item");
        assert_eq!(spelled("Vec<(Item, String)>", &from_util).unwrap(), "Vec<(crate::inv::Item,String)>");
        assert_eq!(spelled("Arc<Mutex<u8>>", &from_util).unwrap(), "::std::sync::Arc<::std::sync::Mutex<u8>>");
        assert_eq!(spelled("alloc::string::String", &from_util).unwrap(), "::std::string::String");
        assert_eq!(spelled("Hidden", &from_inv).unwrap(), "crate::inv::Hidden");
        assert!(spelled("Hidden", &from_util).is_err(), "private to inv");
        assert!(spelled("Inner", &from_util).is_err(), "inside a private module");
        assert_eq!(spelled("Inner", &from_inv).unwrap(), "crate::inv::private::Inner");
        assert_eq!(spelled("Wide", &from_util).unwrap(), "crate::inv::open::Wide");
        assert!(spelled("T", &from_util).is_err());
        assert!(spelled("{closure@src/main.rs:1:1}", &from_util).is_err());
    }
}
