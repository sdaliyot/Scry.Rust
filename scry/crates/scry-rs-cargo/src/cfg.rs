//! `#[cfg(...)]`, evaluated as far as the build's flags tell: which code this build compiles.
//!
//! Cargo passes every enabled feature to rustc as `--cfg feature="..."`, so a `feature = "..."`
//! predicate is known for certain. Anything else (`target_os`, `test`, a custom cfg) is not
//! evaluated, and counts as possibly true, so code is only ever left out when a `#[cfg]` is
//! certainly false - as `#[cfg(not(feature = "server"))]` is in a build with `server` on.

use std::collections::HashSet;

use syn::punctuated::Punctuated;
use syn::{Attribute, Expr, Lit, Meta, Token};

/// The features a build has on.
#[derive(Debug, Clone, Default)]
pub struct Cfg {
    features: HashSet<String>,
}

impl Cfg {
    /// From rustc's arguments: the `feature="..."` values of its `--cfg` flags.
    pub fn from_cfg_flags<'a>(values: impl Iterator<Item = &'a str>) -> Self {
        let features =
            values.filter_map(|value| value.strip_prefix("feature=\"")?.strip_suffix('"')).map(str::to_owned).collect();
        Self { features }
    }

    /// Whether some `#[cfg]` among `attrs` is certainly false, so the item isn't compiled.
    pub fn off(&self, attrs: &[Attribute]) -> bool {
        attrs.iter().any(|attr| {
            attr.path().is_ident("cfg") && attr.parse_args::<Meta>().is_ok_and(|meta| self.eval(&meta) == Some(false))
        })
    }

    /// A cfg predicate's value, or `None` when the build's flags don't tell.
    fn eval(&self, meta: &Meta) -> Option<bool> {
        match meta {
            Meta::NameValue(pair) if pair.path.is_ident("feature") => match &pair.value {
                Expr::Lit(syn::ExprLit { lit: Lit::Str(name), .. }) => Some(self.features.contains(&name.value())),
                _ => None,
            },
            Meta::List(list) => {
                let inner = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated).ok()?;
                let values: Vec<Option<bool>> = inner.iter().map(|meta| self.eval(meta)).collect();
                if list.path.is_ident("not") {
                    match values.as_slice() {
                        [value] => value.map(|value| !value),
                        _ => None,
                    }
                } else if list.path.is_ident("all") {
                    if values.contains(&Some(false)) {
                        Some(false)
                    } else if values.iter().all(|value| *value == Some(true)) {
                        Some(true)
                    } else {
                        None
                    }
                } else if list.path.is_ident("any") {
                    if values.contains(&Some(true)) {
                        Some(true)
                    } else if values.iter().all(|value| *value == Some(false)) {
                        Some(false)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn off(cfg: &Cfg, attr: &str) -> bool {
        let item: syn::ItemFn = syn::parse_str(&format!("{attr} fn f() {{}}")).unwrap();
        cfg.off(&item.attrs)
    }

    #[test]
    fn only_certainly_false_cfgs_turn_code_off() {
        let cfg = Cfg::from_cfg_flags(["feature=\"server\"", "feature=\"app\"", "debug_assertions"].into_iter());
        assert!(off(&cfg, r#"#[cfg(not(feature = "server"))]"#));
        assert!(off(&cfg, r#"#[cfg(feature = "desktop")]"#));
        assert!(off(&cfg, r#"#[cfg(all(not(feature = "server"), feature = "operator"))]"#));
        assert!(off(&cfg, r#"#[cfg(any(feature = "web", feature = "desktop"))]"#));
        assert!(!off(&cfg, r#"#[cfg(feature = "server")]"#));
        assert!(!off(&cfg, r#"#[cfg(all(feature = "server", feature = "app"))]"#));
        // Not evaluated, so possibly true.
        assert!(!off(&cfg, r#"#[cfg(target_os = "windows")]"#));
        assert!(!off(&cfg, r#"#[cfg(all(target_os = "windows", feature = "server"))]"#));
        assert!(!off(&cfg, r#"#[cfg(not(test))]"#));
        assert!(!off(&cfg, "#[inline]"));
        // Certainly false whatever the unknown part is.
        assert!(off(&cfg, r#"#[cfg(all(unix, feature = "web"))]"#));
    }
}
