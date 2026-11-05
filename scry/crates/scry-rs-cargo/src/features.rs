//! The extra `--features` a `cargo scry` build turns on, from `cargo metadata`:
//!
//! - scry-rs's optional lock support (`scry/tokio`) when the crate is in the dependency graph.
//! - The serde feature of each direct dependency that has one (`uuid/serde`, `chrono/serde`), so
//!   its types are read and written as values rather than shown through `Debug`. Serde features
//!   only add impls, so this changes what Scry sees, not what the app does; and it applies to
//!   `cargo scry` builds only, which have their own target directory.
//!
//! ```toml
//! [package.metadata.scry.serde]
//! enabled = true          # the default
//! exclude = ["chrono"]    # dependencies, by the name the manifest gives them
//! ```
//!
//! `[workspace.metadata.scry.serde]` sets the same keys for every crate; a crate's own table
//! overrides each key it sets.
//!
//! cargo rejects `dep/feature` when none of the selected packages has that dependency, so the
//! serde features are worked out for the packages the command selects, the way cargo selects them.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// The `--features` values to add, for a cargo command whose arguments (before any `--`) are
/// `args`, run in `cwd`.
pub fn extra(metadata: &Value, args: &[OsString], cwd: &Path) -> Result<Vec<String>, String> {
    let packages = metadata["packages"].as_array().map(Vec::as_slice).unwrap_or_default();
    let mut features: Vec<String> = ["tokio"]
        .into_iter()
        .filter(|name| packages.iter().any(|package| package["name"] == *name))
        .map(|name| format!("scry/{name}"))
        .collect();
    for id in selected(metadata, args, cwd) {
        let Some(package) = packages.iter().find(|package| package["id"] == id) else { continue };
        let settings = Settings::read(metadata, package)?;
        if !settings.enabled {
            continue;
        }
        for feature in serde_features(metadata, package, &settings.exclude) {
            if !features.contains(&feature) {
                features.push(feature);
            }
        }
    }
    Ok(features)
}

/// The ids of the packages a cargo command with these arguments builds: `-p`/`--package`,
/// `--workspace`, else the package in the current directory, else the workspace's default members.
/// Empty when it cannot tell (a `-p` pattern, `--exclude`), so no serde feature is added.
fn selected(metadata: &Value, args: &[OsString], cwd: &Path) -> Vec<String> {
    let packages = metadata["packages"].as_array().map(Vec::as_slice).unwrap_or_default();
    let members: Vec<&str> =
        metadata["workspace_members"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let mut names = Vec::new();
    let mut workspace = false;
    let mut manifest = None;
    let mut args = args.iter().map(|arg| arg.to_string_lossy());
    while let Some(arg) = args.next() {
        let value = |flag: &str, args: &mut dyn Iterator<Item = std::borrow::Cow<'_, str>>| {
            if arg == flag {
                args.next().map(|value| value.into_owned())
            } else {
                arg.strip_prefix(flag).map(|rest| rest.trim_start_matches('=').to_owned())
            }
        };
        if arg == "--workspace" || arg == "--all" {
            workspace = true;
        } else if arg.starts_with("--exclude") {
            return Vec::new();
        } else if let Some(name) = value("--package", &mut args).or_else(|| value("-p", &mut args)) {
            names.push(name);
        } else if let Some(path) = value("--manifest-path", &mut args) {
            manifest = Some(PathBuf::from(path));
        }
    }
    let member_named = |name: &str| {
        // `name` or `name@version`; a glob pattern matches nothing here, and so selects nothing.
        let name = name.split('@').next().unwrap_or(name);
        members.iter().find(|id| packages.iter().any(|p| p["id"] == **id && p["name"] == name)).map(|id| id.to_string())
    };
    if workspace {
        return members.iter().map(|id| id.to_string()).collect();
    }
    if !names.is_empty() {
        return names.iter().filter_map(|name| member_named(name)).collect();
    }
    let manifest = manifest
        .map(|path| if path.is_absolute() { path } else { cwd.join(path) })
        .or_else(|| cwd.ancestors().map(|dir| dir.join("Cargo.toml")).find(|path| path.is_file()));
    if let Some(manifest) = manifest
        && let Some(package) = packages
            .iter()
            .find(|package| package["manifest_path"].as_str().is_some_and(|path| same_file(Path::new(path), &manifest)))
    {
        return vec![package["id"].as_str().unwrap_or_default().to_owned()];
    }
    let defaults = metadata["workspace_default_members"].as_array().filter(|list| !list.is_empty());
    match defaults {
        Some(list) => list.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
        None => members.iter().map(|id| id.to_string()).collect(),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// `dep/serde` (or `dep?/serde` for an optional dependency) for each of the package's normal
/// dependencies from outside the workspace that has a serde feature it does not turn on itself.
fn serde_features(metadata: &Value, package: &Value, exclude: &[String]) -> Vec<String> {
    let packages = metadata["packages"].as_array().map(Vec::as_slice).unwrap_or_default();
    let members: Vec<&str> =
        metadata["workspace_members"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let node = metadata["resolve"]["nodes"].as_array().into_iter().flatten().find(|node| node["id"] == package["id"]);
    let resolved: Vec<&str> = node
        .and_then(|node| node["deps"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|dep| dep["pkg"].as_str())
        .collect();
    let mut features = Vec::new();
    for dependency in package["dependencies"].as_array().into_iter().flatten() {
        if !dependency["kind"].is_null() {
            continue; // dev and build dependencies hold none of the app's state
        }
        let Some(name) = dependency["name"].as_str() else { continue };
        if name.starts_with("serde") {
            continue; // serde's own crates: their "serde" features are parts of serde, not support for it
        }
        let key = dependency["rename"].as_str().unwrap_or(name);
        if exclude.iter().any(|excluded| excluded == key) {
            continue;
        }
        let Some(target) = packages.iter().find(|candidate| {
            candidate["name"] == name
                && candidate["id"].as_str().is_some_and(|id| resolved.contains(&id) && !members.contains(&id))
        }) else {
            continue;
        };
        let Some(feature) = serde_feature(target) else { continue };
        let on = dependency["features"].as_array().into_iter().flatten().any(|f| f == feature);
        if !on {
            let weak = if dependency["optional"] == true { "?" } else { "" };
            features.push(format!("{key}{weak}/{feature}"));
        }
    }
    features
}

/// The feature that turns on a package's serde support: `serde` when there is one, else the first
/// with `serde` in its name that enables a serde dependency (`serde1`, `with-serde`, ...). Only by
/// name: a feature like dioxus's `fullstack` also pulls in serde, and does a lot more besides.
fn serde_feature(package: &Value) -> Option<&str> {
    let features = package["features"].as_object()?;
    if features.contains_key("serde") {
        return Some("serde");
    }
    let enables_serde = |values: &Value| {
        values.as_array().into_iter().flatten().filter_map(Value::as_str).any(|value| {
            let dependency = value.strip_prefix("dep:").unwrap_or(value);
            dependency.starts_with("serde") && !dependency.contains('/')
        })
    };
    let mut names: Vec<&String> = features
        .iter()
        .filter(|(name, values)| name.contains("serde") && enables_serde(values))
        .map(|(name, _)| name)
        .collect();
    names.sort();
    names.first().map(|name| name.as_str())
}

struct Settings {
    enabled: bool,
    exclude: Vec<String>,
}

impl Settings {
    fn read(metadata: &Value, package: &Value) -> Result<Self, String> {
        let mut settings = Self { enabled: true, exclude: Vec::new() };
        settings.apply(&metadata["metadata"]["scry"]["serde"], "[workspace.metadata.scry.serde]")?;
        let place = format!("{} [package.metadata.scry.serde]", package["manifest_path"].as_str().unwrap_or_default());
        settings.apply(&package["metadata"]["scry"]["serde"], &place)?;
        Ok(settings)
    }

    fn apply(&mut self, table: &Value, place: &str) -> Result<(), String> {
        let Some(table) = table.as_object() else { return Ok(()) };
        for (key, value) in table {
            match key.as_str() {
                "enabled" => {
                    self.enabled = value.as_bool().ok_or_else(|| format!("{place}: `enabled` must be true or false"))?
                }
                "exclude" => {
                    self.exclude = value
                        .as_array()
                        .and_then(|list| list.iter().map(|name| name.as_str().map(str::to_owned)).collect())
                        .ok_or_else(|| format!("{place}: `exclude` must be a list of dependency names"))?
                }
                _ => return Err(format!("{place}: unknown key `{key}`")),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn metadata(app_metadata: Value) -> Value {
        json!({
            "workspace_members": ["app", "other"],
            "workspace_default_members": ["app", "other"],
            "packages": [
                {
                    "id": "app", "name": "app", "manifest_path": "/none/app/Cargo.toml", "metadata": app_metadata,
                    "dependencies": [
                        { "name": "uuid", "kind": null, "rename": null, "optional": false, "features": [] },
                        { "name": "chrono", "kind": null, "rename": "time", "optional": true, "features": [] },
                        { "name": "plain", "kind": null, "rename": null, "optional": false, "features": [] },
                        { "name": "on", "kind": null, "rename": null, "optional": false, "features": ["serde"] },
                        { "name": "testy", "kind": "dev", "rename": null, "optional": false, "features": [] },
                        { "name": "serde", "kind": null, "rename": null, "optional": false, "features": [] },
                    ]
                },
                { "id": "other", "name": "other", "manifest_path": "/none/other/Cargo.toml", "dependencies": [] },
                { "id": "uuid", "name": "uuid", "features": { "serde": ["dep:serde"], "v4": [] } },
                { "id": "chrono", "name": "chrono", "features": { "serde1": ["dep:serde"] } },
                { "id": "plain", "name": "plain", "features": { "std": [], "web": ["dep:serde"] } },
                { "id": "on", "name": "on", "features": { "serde": ["dep:serde"] } },
                { "id": "testy", "name": "testy", "features": { "serde": ["dep:serde"] } },
                { "id": "tokio", "name": "tokio", "features": {} },
                { "id": "serde", "name": "serde", "features": { "serde_derive": ["dep:serde_derive"] } },
            ],
            "resolve": { "nodes": [
                { "id": "app", "deps": [
                    { "pkg": "uuid" }, { "pkg": "chrono" }, { "pkg": "plain" }, { "pkg": "on" }, { "pkg": "testy" },
                    { "pkg": "serde" },
                ] },
            ] },
        })
    }

    fn args(text: &str) -> Vec<OsString> {
        text.split_whitespace().map(OsString::from).collect()
    }

    #[test]
    fn turns_on_serde_features_of_direct_dependencies() {
        let features = extra(&metadata(Value::Null), &args("run -p app"), Path::new("/none")).unwrap();
        assert_eq!(features, ["scry/tokio", "uuid/serde", "time?/serde1"]);
    }

    #[test]
    fn only_for_selected_packages() {
        let metadata = metadata(Value::Null);
        let cwd = Path::new("/none");
        assert_eq!(extra(&metadata, &args("run -p other"), cwd).unwrap(), ["scry/tokio"]);
        assert_eq!(extra(&metadata, &args("run --package=app"), cwd).unwrap().len(), 3);
        assert_eq!(extra(&metadata, &args("build --workspace"), cwd).unwrap().len(), 3);
        assert_eq!(extra(&metadata, &args("build --workspace --exclude other"), cwd).unwrap(), ["scry/tokio"]);
        assert_eq!(extra(&metadata, &args("build -p ap*"), cwd).unwrap(), ["scry/tokio"]);
        // No package in or above the directory: cargo builds the default members.
        assert_eq!(extra(&metadata, &args("build"), Path::new("/no/such/dir")).unwrap().len(), 3);
    }

    #[test]
    fn settings_turn_it_off_or_exclude_dependencies() {
        let cwd = Path::new("/none");
        let off = metadata(json!({ "scry": { "serde": { "enabled": false } } }));
        assert_eq!(extra(&off, &args("run -p app"), cwd).unwrap(), ["scry/tokio"]);
        let some = metadata(json!({ "scry": { "serde": { "exclude": ["uuid"] } } }));
        assert_eq!(extra(&some, &args("run -p app"), cwd).unwrap(), ["scry/tokio", "time?/serde1"]);
        let mut workspace = metadata(json!({ "scry": { "serde": { "enabled": true } } }));
        workspace["metadata"] = json!({ "scry": { "serde": { "enabled": false, "exclude": ["uuid"] } } });
        assert_eq!(extra(&workspace, &args("run -p app"), cwd).unwrap(), ["scry/tokio", "time?/serde1"]);
        let typo = metadata(json!({ "scry": { "serde": { "exclud": [] } } }));
        assert!(extra(&typo, &args("run -p app"), cwd).unwrap_err().contains("unknown key `exclud`"));
    }
}
