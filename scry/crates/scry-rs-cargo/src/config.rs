//! What a crate's manifest says about how `cargo scry` treats it:
//!
//! ```toml
//! [package.metadata.scry.hooks]
//! enabled = true                       # the default
//! exclude = ["crate::util::*", "crate::inventory::Inventory::len"]
//! ```
//!
//! `[workspace.metadata.scry.hooks]` in the workspace's manifest sets the same keys for every crate
//! in it; a crate's own table overrides each key it sets.
//!
//! ```toml
//! [package.metadata.scry]
//! transparent-attributes = ["my_retry", "metrics::timed"]
//! ```
//!
//! names attribute macros that leave a function callable as written, so it still gets a call shim
//! and a hook point (by default a function under an unknown attribute macro gets neither). A name
//! matches an attribute's last path segment or its whole path. The workspace's list and the
//! crate's are combined.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Config {
    pub hooks: bool,
    hook_exclude: Vec<Vec<String>>,
    /// Attribute macros that leave a function as written; see the module docs.
    pub transparent: Vec<String>,
    /// The manifests read, which the build has to be redone for when they change.
    pub manifests: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self { hooks: true, hook_exclude: Vec::new(), transparent: Vec::new(), manifests: Vec::new() }
    }
}

impl Config {
    /// Reads the package's manifest in `manifest_dir`, and the workspace manifest above it.
    pub fn load(manifest_dir: &Path) -> Result<Self, String> {
        let mut config = Self::default();
        let package = manifest_dir.join("Cargo.toml");
        let workspace = manifest_dir.ancestors().map(|dir| dir.join("Cargo.toml")).find(|path| {
            fs::read_to_string(path)
                .ok()
                .and_then(|text| text.parse::<toml::Table>().ok())
                .is_some_and(|table| table.contains_key("workspace"))
        });
        if let Some(workspace) = workspace {
            config.apply(&workspace, &["workspace", "metadata", "scry", "hooks"])?;
        }
        config.apply(&package, &["package", "metadata", "scry", "hooks"])?;
        Ok(config)
    }

    fn apply(&mut self, manifest: &Path, keys: &[&str]) -> Result<(), String> {
        let Ok(text) = fs::read_to_string(manifest) else { return Ok(()) };
        if !self.manifests.contains(&manifest.to_path_buf()) {
            self.manifests.push(manifest.to_path_buf());
        }
        let table: toml::Table = text.parse().map_err(|error| format!("{}: {error}", manifest.display()))?;
        let mut value = Some(&table);
        let mut scry = None;
        for key in keys {
            value = value.and_then(|table| table.get(*key)).and_then(toml::Value::as_table);
            if *key == "scry" {
                scry = value;
            }
        }
        if let Some(list) = scry.and_then(|scry| scry.get("transparent-attributes")) {
            let place = format!("{} [{}]", manifest.display(), keys[..keys.len() - 1].join("."));
            let names = list
                .as_array()
                .and_then(|list| list.iter().map(|name| name.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>())
                .ok_or_else(|| format!("{place}: `transparent-attributes` must be a list of attribute names"))?;
            for name in names {
                if !self.transparent.contains(&name) {
                    self.transparent.push(name);
                }
            }
        }
        let Some(hooks) = value else { return Ok(()) };
        let place = || format!("{} [{}]", manifest.display(), keys.join("."));
        if let Some(enabled) = hooks.get("enabled") {
            self.hooks = enabled.as_bool().ok_or_else(|| format!("{}: `enabled` must be true or false", place()))?;
        }
        if let Some(exclude) = hooks.get("exclude") {
            let list = exclude.as_array().ok_or_else(|| format!("{}: `exclude` must be a list of paths", place()))?;
            self.hook_exclude = list
                .iter()
                .map(|pattern| {
                    pattern
                        .as_str()
                        .map(pattern_segments)
                        .ok_or_else(|| format!("{}: `exclude` must hold strings", place()))
                })
                .collect::<Result<_, _>>()?;
        }
        if let Some(key) = hooks.keys().find(|key| !matches!(key.as_str(), "enabled" | "exclude")) {
            return Err(format!("{}: unknown key `{key}`", place()));
        }
        Ok(())
    }

    /// Whether a function, by its crate-relative segments (`["crate", "util", "ratio"]`), is
    /// excluded from hooks.
    pub fn hook_excluded(&self, path: &[String]) -> bool {
        self.hook_exclude.iter().any(|pattern| matches(pattern, path))
    }
}

/// `crate::util::*` as segments; a leading `crate` is optional.
fn pattern_segments(pattern: &str) -> Vec<String> {
    let mut segments: Vec<String> = pattern.split("::").map(|segment| segment.trim().to_owned()).collect();
    if segments.first().is_some_and(|first| first == "crate") {
        segments.remove(0);
    }
    segments
}

/// `*` as the last segment matches one or more segments; elsewhere it matches exactly one.
fn matches(pattern: &[String], path: &[String]) -> bool {
    let path = match path.first() {
        Some(first) if first == "crate" => &path[1..],
        _ => path,
    };
    match pattern.split_last() {
        Some((last, prefix)) if last == "*" => {
            path.len() > prefix.len()
                && prefix.iter().zip(path).all(|(pattern, segment)| pattern == "*" || pattern == segment)
        }
        _ => {
            pattern.len() == path.len()
                && pattern.iter().zip(path).all(|(pattern, segment)| pattern == "*" || pattern == segment)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> Vec<String> {
        text.split("::").map(str::to_owned).collect()
    }

    #[test]
    fn exclude_patterns_match_modules_and_functions() {
        let config = Config {
            hook_exclude: vec![pattern_segments("crate::util::*"), pattern_segments("inventory::Inventory::len")],
            ..Config::default()
        };
        assert!(config.hook_excluded(&path("crate::util::ratio")));
        assert!(config.hook_excluded(&path("crate::util::Cache::get")));
        assert!(!config.hook_excluded(&path("crate::util")));
        assert!(config.hook_excluded(&path("crate::inventory::Inventory::len")));
        assert!(!config.hook_excluded(&path("crate::inventory::Inventory::add")));
    }

    #[test]
    fn package_settings_override_the_workspace() {
        let dir = std::env::temp_dir().join(format!("scry-config-{}", std::process::id()));
        let member = dir.join("member");
        fs::create_dir_all(&member).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\n[workspace.metadata.scry.hooks]\nenabled = false\nexclude = [\"crate::a::*\"]\n",
        )
        .unwrap();
        fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"m\"\n[package.metadata.scry.hooks]\nenabled = true\n",
        )
        .unwrap();
        let config = Config::load(&member).unwrap();
        assert!(config.hooks, "the package's own `enabled` wins");
        assert!(config.transparent.is_empty());
        assert!(config.hook_excluded(&path("crate::a::f")), "the workspace's `exclude` still applies");
        assert_eq!(config.manifests.len(), 2);

        fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"m\"\n[package.metadata.scry]\ntransparent-attributes = [\"retry\", \"m::timed\"]\n",
        )
        .unwrap();
        assert_eq!(Config::load(&member).unwrap().transparent, ["retry", "m::timed"]);

        fs::write(member.join("Cargo.toml"), "[package]\nname = \"m\"\n[package.metadata.scry.hooks]\nenable = true\n")
            .unwrap();
        assert!(Config::load(&member).unwrap_err().contains("unknown key `enable`"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
