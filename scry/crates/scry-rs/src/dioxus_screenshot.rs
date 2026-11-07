//! `screenshot [selector]`: the page of a Dioxus desktop window as its webview renders it, as a
//! PNG - the whole visible page, or one element (a CSS selector or `text=Label`, as for `click`).
//!
//! Nothing is read off the screen. The webview's engine renders the page for the capture
//! (Chromium's `Page.captureScreenshot`, through WebView2's DevTools protocol), so other windows
//! over it, its position on the desktop, and whether a display is attached do not matter. An
//! element is captured in full even where it runs past the bottom of the window. A webview stops
//! drawing while its window is minimized (or the app hides it), so for a capture it is shown
//! again, and hidden again after if the app had hidden it.
//!
//! The reply is `{format: "png", width, height, base64Data}` (width and height in pixels), plus
//! `element` - what was captured - for a selector. `scry-rs --out shot.png screenshot ...` writes the
//! PNG to a file instead of printing it.
//!
//! WebView2 only, so Windows only for now; WKWebView (macOS) and WebKitGTK (Linux) each have a
//! snapshot API of their own.

use serde_json::{Map, Value, json};

use crate::Error;

pub(crate) const VERB: &str = "screenshot";

/// What part of the page to capture: one element, or the visible page.
pub(crate) enum Capture {
    Page,
    Element(String),
}

/// `screenshot [selector]`, or `None` for another command.
pub(crate) fn parse(line: &str) -> Option<Result<Capture, Error>> {
    let line = line.trim();
    let (verb, rest) = line.split_once(char::is_whitespace).map_or((line, ""), |(verb, rest)| (verb, rest.trim()));
    if verb != VERB {
        return None;
    }
    if rest.is_empty() {
        return Some(Ok(Capture::Page));
    }
    Some(crate::dioxus_dom::whole_argument(rest, "a selector").map(Capture::Element))
}

/// A box on the page, in CSS pixels from the top left of the document.
#[derive(Debug, PartialEq)]
pub(crate) struct Clip {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Whether it reaches past the window's viewport (so the page has to be rendered beyond it).
    pub beyond: bool,
}

impl Clip {
    /// The box the page script for an element answers with (`dioxus_dom::rect_script`).
    pub(crate) fn from_rect(rect: &Value) -> Result<Clip, Error> {
        let number = |key: &str| {
            rect[key].as_f64().ok_or_else(|| Error::Script(format!("the element's box has no {key}: {rect}")))
        };
        let clip = Clip {
            x: number("x")?,
            y: number("y")?,
            width: number("width")?,
            height: number("height")?,
            beyond: rect["inside"] != true,
        };
        if clip.width < 1.0 || clip.height < 1.0 {
            return Err(Error::BadCommand(format!(
                "the element takes up no space on the page ({} x {}), so there is nothing to capture",
                clip.width, clip.height
            )));
        }
        Ok(clip)
    }
}

/// The DevTools protocol's `Page.captureScreenshot` parameters. Always with a clip: without one,
/// Chromium waits for a fresh frame, which a minimized window never draws. An element is captured
/// in full, rendered beyond the window's viewport if it reaches past it - only then, for the same
/// reason: that lays the page out again at full size and waits for a frame.
pub(crate) fn parameters(clip: &Clip) -> Value {
    json!({
        "format": "png",
        "captureBeyondViewport": clip.beyond,
        "clip": { "x": clip.x, "y": clip.y, "width": clip.width, "height": clip.height, "scale": 1 },
    })
}

/// The reply for the protocol's answer, `{"data": "<base64 PNG>"}`.
pub(crate) fn reply(answer: &str, element: Option<Value>) -> Result<Value, Error> {
    let answer: Value =
        serde_json::from_str(answer).map_err(|error| Error::Script(format!("unreadable screenshot: {error}")))?;
    let data = answer["data"].as_str().ok_or_else(|| Error::Script(format!("the webview sent no image: {answer}")))?;
    let (width, height) = png_size(data).ok_or_else(|| Error::Script("the webview's image is not a PNG".into()))?;
    let mut reply = Map::new();
    reply.insert("format".into(), "png".into());
    reply.insert("width".into(), width.into());
    reply.insert("height".into(), height.into());
    if let Some(element) = element {
        reply.insert("element".into(), element);
    }
    reply.insert("base64Data".into(), data.into());
    Ok(Value::Object(reply))
}

/// A PNG's width and height, from its header (the first 24 bytes, 32 characters of base64).
fn png_size(base64: &str) -> Option<(u32, u32)> {
    let header = decode_base64(base64.get(..32)?)?;
    if header.get(..8)? != b"\x89PNG\r\n\x1a\n" || header.get(12..16)? != b"IHDR" {
        return None;
    }
    let word = |at: usize| Some(u32::from_be_bytes(header.get(at..at + 4)?.try_into().ok()?));
    Some((word(16)?, word(20)?))
}

fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len() / 4 * 3);
    let mut bits = 0u32;
    let mut count = 0;
    for c in text.bytes().filter(|c| *c != b'=') {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            bytes.push((bits >> count) as u8);
        }
    }
    Some(bytes)
}

/// The capture under way in a window's webview.
pub(crate) struct Pending {
    answer: futures_channel::oneshot::Receiver<Result<String, String>>,
    #[cfg(windows)]
    view: View,
}

/// How long a capture may wait for a frame before the webview is woken again.
const WAKE_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// Asks the webview of the window whose runtime is current to render `clip`.
#[cfg(windows)]
pub(crate) fn start(clip: &Clip) -> Result<Pending, Error> {
    use dioxus_desktop::wry::WebViewExtWindows;
    use webview2_com::CallDevToolsProtocolMethodCompletedHandler;
    use windows_core::HSTRING;

    let desktop = dioxus_core::try_consume_context::<dioxus_desktop::DesktopContext>()
        .ok_or_else(|| Error::BadCommand("screenshot needs a Dioxus desktop window".into()))?;
    let controller = desktop.webview.controller();
    let mut visible = windows_core::BOOL::default();
    unsafe { controller.IsVisible(&mut visible) }.map_err(com_error)?;
    let view = View { controller, window: desktop.window.clone(), hidden: !visible.as_bool() };
    view.wake();

    let (sender, answer) = futures_channel::oneshot::channel();
    let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |result, json| {
        let _ = sender.send(result.map(|()| json.clone()).map_err(|error| devtools_error(&error, &json)));
        Ok(())
    }));
    let parameters = HSTRING::from(parameters(clip).to_string());
    let started = unsafe {
        desktop.webview.webview().CallDevToolsProtocolMethod(
            &HSTRING::from("Page.captureScreenshot"),
            &parameters,
            &handler,
        )
    };
    if let Err(error) = started {
        view.restore();
        return Err(com_error(error));
    }
    Ok(Pending { answer, view })
}

#[cfg(not(windows))]
pub(crate) fn start(_: &Clip) -> Result<Pending, Error> {
    Err(Error::BadCommand(
        "screenshot is only supported on Windows (WebView2) so far; macOS and Linux webviews are not covered yet"
            .into(),
    ))
}

impl Pending {
    /// Waits for the image, waking the webview while it waits (see [`View::wake`]), then hides it
    /// again if it was hidden.
    pub(crate) async fn finish(self, element: Option<Value>) -> Result<Value, Error> {
        use futures_util::future::{Either, select};

        let Pending {
            mut answer,
            #[cfg(windows)]
            view,
        } = self;
        let started = std::time::Instant::now();
        let answer = loop {
            match select(&mut answer, timer(WAKE_EVERY)).await {
                Either::Left((answer, _)) => break Ok(answer),
                Either::Right(_) if started.elapsed() >= crate::run_async::CALL_TIMEOUT => {
                    break Err(Error::Timeout(format!(
                        "the webview did not render the page within {}s",
                        crate::run_async::CALL_TIMEOUT.as_secs_f32()
                    )));
                }
                Either::Right(_) => {
                    #[cfg(windows)]
                    view.wake();
                }
            }
        };
        #[cfg(windows)]
        view.restore();
        match answer? {
            Ok(Ok(json)) => reply(&json, element),
            Ok(Err(error)) => Err(Error::Script(format!("the webview could not capture the page: {error}"))),
            Err(_) => Err(Error::Script("the page went away before it was captured (its window closed?)".into())),
        }
    }
}

/// Fires once after `delay`.
fn timer(delay: std::time::Duration) -> futures_channel::oneshot::Receiver<()> {
    let (sender, receiver) = futures_channel::oneshot::channel();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let _ = sender.send(());
    });
    receiver
}

/// A window's webview, as far as a capture needs it.
#[cfg(windows)]
struct View {
    controller: webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller,
    window: std::sync::Arc<dioxus_desktop::tao::window::Window>,
    /// Whether the app had hidden the webview (the controller's IsVisible) before the capture.
    hidden: bool,
}

#[cfg(windows)]
impl View {
    /// A webview stops drawing when it is hidden: by the app, or by Chromium itself while its
    /// window is minimized. A capture then waits for a frame that never comes. Showing it - hiding
    /// and showing it again, if it is shown already - has it draw one.
    fn wake(&self) {
        let minimized = self.window.is_minimized();
        if minimized && !self.hidden {
            let _ = unsafe { self.controller.SetIsVisible(false) };
        }
        if minimized || self.hidden {
            let _ = unsafe { self.controller.SetIsVisible(true) };
        }
    }

    /// Hides the webview again if the app had hidden it.
    fn restore(&self) {
        if self.hidden {
            let _ = unsafe { self.controller.SetIsVisible(false) };
        }
    }
}

#[cfg(windows)]
fn com_error(error: windows_core::Error) -> Error {
    Error::Script(format!("WebView2: {error}"))
}

/// The protocol's error, `{"code": ..., "message": ...}`, or the failed call's.
#[cfg(windows)]
fn devtools_error(error: &windows_core::Error, json: &str) -> String {
    serde_json::from_str::<Value>(json)
        .ok()
        .and_then(|value| value["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1 x 1 PNG.
    const PIXEL: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    #[test]
    fn commands_name_the_page_or_an_element() {
        assert!(parse("get ui").is_none());
        assert!(matches!(parse("screenshot"), Some(Ok(Capture::Page))));
        assert!(
            matches!(parse("screenshot text=Save changes"), Some(Ok(Capture::Element(s))) if s == "text=Save changes")
        );
        assert!(matches!(parse("screenshot \"nav a\""), Some(Ok(Capture::Element(s))) if s == "nav a"));
    }

    #[test]
    fn elements_are_clipped_to_their_box() {
        let clip =
            Clip::from_rect(&json!({ "x": 10.5, "y": 2000, "width": 300, "height": 40, "inside": false })).unwrap();
        let parameters = parameters(&clip);
        assert_eq!(parameters["clip"], json!({ "x": 10.5, "y": 2000.0, "width": 300.0, "height": 40.0, "scale": 1 }));
        assert_eq!(parameters["captureBeyondViewport"], true);
        let inside = Clip::from_rect(&json!({ "x": 0, "y": 0, "width": 300, "height": 40, "inside": true })).unwrap();
        assert_eq!(super::parameters(&inside)["captureBeyondViewport"], false);
        assert!(Clip::from_rect(&json!({ "x": 0, "y": 0, "width": 0, "height": 40 })).is_err());
    }

    #[test]
    fn replies_carry_the_png_and_its_size() {
        let reply = reply(&json!({ "data": PIXEL }).to_string(), None).unwrap();
        assert_eq!((reply["width"].as_u64(), reply["height"].as_u64()), (Some(1), Some(1)));
        assert_eq!(reply["base64Data"], PIXEL);
        assert!(super::reply(r#"{"data":"aGVsbG8gd29ybGQgdGhpcyBpcyBub3QgYSBwbmc="}"#, None).is_err());
    }
}
