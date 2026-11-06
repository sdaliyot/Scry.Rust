//! The app's router, under `ui`: `ui.route` is the current route as the URL the router shows
//! (`/tasks/3?sort=asc`), and setting it navigates there, as a `Link` would. `ui.routes` lists the
//! route patterns the app declares (`/tasks/:id`), read from the router's site map.
//!
//! Nothing here knows the app's route type. Dioxus keeps a type-erased handle on the router at the
//! root of the virtual dom ([`root_router`]), which reads and pushes routes as strings, and every
//! `#[derive(Routable)]` enum publishes its site map through it.
//!
//! The router's history is reachable as well: `ui.can_go_back` and `ui.can_go_forward` read it, and
//! the built-in functions `scry::ui::back` and `scry::ui::forward` (`scry call ui::back`, `back()`
//! in a script) step through it, as the browser's buttons would.

use dioxus_router::{NavigationTarget, RouterContext, SegmentType, SiteMapSegment, root_router};
use serde_json::Value;

use crate::call::{Function, Receiver};
use crate::{Error, Reflect};

pub(crate) const ROUTE: &str = "route";
pub(crate) const ROUTES: &str = "routes";
pub(crate) const CAN_GO_BACK: &str = "can_go_back";
pub(crate) const CAN_GO_FORWARD: &str = "can_go_forward";

/// The members the router adds to `ui`, with their values.
pub(crate) fn members(router: RouterContext) -> [(&'static str, Box<dyn Reflect>); 4] {
    [
        (ROUTE, Box::new(current(router))),
        (ROUTES, Box::new(Patterns(patterns(router)))),
        (CAN_GO_BACK, Box::new(router.can_go_back())),
        (CAN_GO_FORWARD, Box::new(router.can_go_forward())),
    ]
}

#[linkme::distributed_slice(crate::__private::FUNCTIONS)]
#[linkme(crate = linkme)]
static BACK: Function = Function {
    name: "back",
    path: "scry::ui::back",
    signature: "fn back() -> String  // the previous route, as the browser's Back button",
    receiver: Receiver::None,
    self_type: None,
    trait_name: None,
    arity: 0,
    invoke: |_, _, _| step(Direction::Back),
};

#[linkme::distributed_slice(crate::__private::FUNCTIONS)]
#[linkme(crate = linkme)]
static FORWARD: Function = Function {
    name: "forward",
    path: "scry::ui::forward",
    signature: "fn forward() -> String  // the next route, as the browser's Forward button",
    receiver: Receiver::None,
    self_type: None,
    trait_name: None,
    arity: 0,
    invoke: |_, _, _| step(Direction::Forward),
};

enum Direction {
    Back,
    Forward,
}

/// Goes one step back or forward in the router's history, returning the route it lands on.
fn step(direction: Direction) -> Result<Value, Error> {
    let router = router().ok_or_else(|| Error::BadCommand("the app has no Dioxus router".into()))?;
    match direction {
        Direction::Back if router.can_go_back() => router.go_back(),
        Direction::Forward if router.can_go_forward() => router.go_forward(),
        Direction::Back => return Err(Error::BadCommand("there is no previous route to go back to".into())),
        Direction::Forward => return Err(Error::BadCommand("there is no next route to go forward to".into())),
    }
    Ok(Value::String(current(router)))
}

/// The router of the app's virtual dom, if it has one. Only on the UI thread, inside the runtime.
pub(crate) fn router() -> Option<RouterContext> {
    dioxus_core::Runtime::try_current()?;
    root_router()
}

/// `ui.route`.
pub(crate) fn current(router: RouterContext) -> String {
    router.full_route_string()
}

/// `ui.routes`: each pattern the app's route type declares.
pub(crate) fn patterns(router: RouterContext) -> Vec<String> {
    let mut patterns: Vec<String> = Vec::new();
    for route in router.site_map().iter().flat_map(SiteMapSegment::flatten) {
        let pattern = format_pattern(&route);
        if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
    }
    patterns
}

/// Reads `ui.route` into a string, lets `visit` change it, and navigates if it did.
pub(crate) fn write(router: RouterContext, visit: crate::VisitMut<'_>) -> Result<(), Error> {
    let before = current(router);
    let mut target = before.clone();
    visit(&mut target)?;
    if target != before {
        navigate(router, &target)?;
    }
    Ok(())
}

fn navigate(router: RouterContext, target: &str) -> Result<(), Error> {
    if !target.starts_with('/') {
        return Err(Error::not_writable(format!(
            "'{target}' is not a route of the app; a route starts with '/' (see ui.routes)"
        )));
    }
    let routes: Vec<Vec<SegmentType>> = router.site_map().iter().flat_map(SiteMapSegment::flatten).collect();
    if !routes.is_empty() && !routes.iter().any(|route| matches(route, target)) {
        let known: Vec<String> = routes.iter().map(|route| format_pattern(route)).collect();
        return Err(Error::not_writable(format!("'{target}' matches none of the app's routes: {}", known.join(", "))));
    }
    match router.push(NavigationTarget::<String>::Internal(target.to_owned())) {
        None => Ok(()),
        Some(failure) => Err(Error::not_writable(format!("the router refused '{target}': {}", failure.0))),
    }
}

/// `/tasks/:id`, `/files/:..path`; `/` for the root.
fn format_pattern(route: &[SegmentType]) -> String {
    let pattern: String = route
        .iter()
        .filter(|segment| !matches!(segment, SegmentType::Static("")))
        .map(|segment| match segment {
            SegmentType::Child => "/:..child".to_owned(),
            other => other.to_string(),
        })
        .collect();
    if pattern.is_empty() { "/".to_owned() } else { pattern }
}

/// Whether the path of `target` (its query and fragment ignored) fits `route`'s segments. A
/// dynamic segment takes any one path segment; whether its text parses is up to the app's route
/// type, which shows its own error page if it doesn't.
fn matches(route: &[SegmentType], target: &str) -> bool {
    let path = target.split(['?', '#']).next().unwrap_or_default();
    let mut parts = path.split('/').filter(|part| !part.is_empty());
    for segment in route {
        match segment {
            SegmentType::Static("") => {}
            SegmentType::Static(text) => {
                if parts.next() != Some(*text) {
                    return false;
                }
            }
            SegmentType::Dynamic(_) => {
                if parts.next().is_none() {
                    return false;
                }
            }
            SegmentType::CatchAll(_) | SegmentType::Child => return true,
            // A kind of segment added after this was written: let the router decide.
            _ => return true,
        }
    }
    parts.next().is_none()
}

/// `ui.routes` as a value.
pub(crate) struct Patterns(pub Vec<String>);

impl Reflect for Patterns {
    fn type_name(&self) -> &'static str {
        "routes"
    }

    fn kind(&self) -> crate::Kind {
        self.0.kind()
    }

    fn members(&self) -> Vec<crate::Member> {
        self.0.members()
    }

    fn snapshot(&self, depth: usize) -> Value {
        self.0.snapshot(depth)
    }

    fn read(&self, path: &[crate::Segment], visit: crate::Visit<'_>) -> Result<(), Error> {
        self.0.read(path, visit)
    }

    fn write(&mut self, _: &[crate::Segment], _: crate::VisitMut<'_>) -> Result<(), Error> {
        Err(Error::not_writable(
            "ui.routes lists the routes the app declares; set ui.route to navigate, or call ui::back or ui::forward",
        ))
    }

    fn write_shared(&self, _: &[crate::Segment], _: crate::VisitMut<'_>) -> Result<(), Error> {
        Err(Error::not_writable(
            "ui.routes lists the routes the app declares; set ui.route to navigate, or call ui::back or ui::forward",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: SegmentType = SegmentType::Static("");

    #[test]
    fn routes_match_by_their_segments() {
        let tasks = [SegmentType::Static("tasks"), SegmentType::Dynamic("id")];
        assert!(matches(&tasks, "/tasks/3"));
        assert!(matches(&tasks, "/tasks/3/?sort=asc#top"));
        assert!(!matches(&tasks, "/tasks"));
        assert!(!matches(&tasks, "/tasks/3/edit"));
        assert!(!matches(&tasks, "/other/3"));
        assert!(matches(&[ROOT], "/"));
        assert!(!matches(&[ROOT], "/x"));
        let files = [SegmentType::Static("files"), SegmentType::CatchAll("path")];
        assert!(matches(&files, "/files"));
        assert!(matches(&files, "/files/a/b"));
        assert!(matches(&[SegmentType::Static("admin"), SegmentType::Child], "/admin/users/7"));
    }

    #[test]
    fn patterns_read_like_the_route_attributes() {
        assert_eq!(format_pattern(&[ROOT]), "/");
        assert_eq!(format_pattern(&[ROOT, SegmentType::Static("tasks"), SegmentType::Dynamic("id")]), "/tasks/:id");
        assert_eq!(format_pattern(&[SegmentType::CatchAll("path")]), "/:..path");
    }
}
