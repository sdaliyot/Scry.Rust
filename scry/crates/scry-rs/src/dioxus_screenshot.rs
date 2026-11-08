//! `screenshot [selector]`: the page of a Dioxus desktop window as its webview renders it, as a
//! PNG - the whole visible page, or one element (a CSS selector or `text=Label`, as for `click`).
//!
//! Nothing is read off the screen. The webview's engine renders the page for the capture, so other
//! windows over it, its position on the desktop, and whether a display is attached do not matter.
//! An element is captured in full even where it runs past the bottom of the window.
//!
//! - **Windows:** Chromium's `Page.captureScreenshot`, through WebView2's DevTools protocol. A
//!   webview stops drawing while its window is minimized (or the app hides it), so for a capture
//!   it is shown again, and hidden again after if the app had hidden it.
//! - **Linux:** WebKitGTK's snapshot of the whole document, cut to the page's or the element's box.
//! - **macOS:** WKWebView's `takeSnapshotWithConfiguration:` of the box, in the view's coordinates,
//!   encoded to PNG through `NSBitmapImageRep`. That snapshot only renders what lies within the
//!   view's bounds and leaves the rest blank, so a box that reaches past the window's viewport (an
//!   element taller than the window: one merely below it is scrolled into view first) is taken with
//!   `createPDFWithConfiguration:` instead, cut to the box in document coordinates, and drawn into a
//!   bitmap at the window's scale. Both keep a Retina screen's resolution, and both work while the
//!   window is covered.
//!
//! The reply is `{format: "png", width, height, base64Data}` (width and height in device pixels),
//! plus `element` - what was captured - for a selector. `scry-rs --out shot.png screenshot ...`
//! writes the PNG to a file instead of printing it.

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
#[derive(Debug, PartialEq, Clone, Copy)]
pub(crate) struct Clip {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Where its top left corner is in the window's viewport, in CSS pixels: `x` and `y` less the
    /// page's scroll offset.
    pub left: f64,
    pub top: f64,
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
            left: number("left")?,
            top: number("top")?,
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

/// The box as WKWebView's snapshot takes it: in the view's coordinates, in points - where it is in
/// the window's viewport, the CSS pixels scaled by the page zoom. AppKit renders points at the
/// screen's backing scale.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // Only WKWebView takes these.
pub(crate) fn view_rect(clip: &Clip, zoom: f64) -> [f64; 4] {
    [clip.left * zoom, clip.top * zoom, clip.width * zoom, clip.height * zoom]
}

/// The box as WKWebView's PDF takes it: in the document's coordinates, in points.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn document_rect(clip: &Clip, zoom: f64) -> [f64; 4] {
    [clip.x * zoom, clip.y * zoom, clip.width * zoom, clip.height * zoom]
}

/// The DevTools protocol's `Page.captureScreenshot` parameters. Always with a clip: without one,
/// Chromium waits for a fresh frame, which a minimized window never draws. An element is captured
/// in full, rendered beyond the window's viewport if it reaches past it - only then, for the same
/// reason: that lays the page out again at full size and waits for a frame.
#[cfg_attr(not(windows), allow(dead_code))] // Only WebView2 takes these so far.
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

#[cfg_attr(windows, allow(dead_code))] // WebView2 answers in base64 itself.
fn encode_base64(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word = chunk.iter().enumerate().fold(0u32, |word, (at, byte)| word | u32::from(*byte) << (16 - 8 * at));
        for at in 0..4 {
            text.push(if at <= chunk.len() { DIGITS[(word >> (18 - 6 * at)) as usize & 63] as char } else { '=' });
        }
    }
    text
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

/// Asks the webview of the window whose runtime is current to render the document, and cuts
/// `clip` out of it.
#[cfg(target_os = "linux")]
pub(crate) fn start(clip: &Clip) -> Result<Pending, Error> {
    use dioxus_desktop::wry::WebViewExtUnix;
    use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt};

    let desktop = dioxus_core::try_consume_context::<dioxus_desktop::DesktopContext>()
        .ok_or_else(|| Error::BadCommand("screenshot needs a Dioxus desktop window".into()))?;
    let (sender, answer) = futures_channel::oneshot::channel();
    let clip = *clip;
    desktop.webview.webview().snapshot(
        SnapshotRegion::FullDocument,
        SnapshotOptions::NONE,
        None::<&webkit2gtk::gio::Cancellable>,
        move |surface| {
            let png = surface.map_err(|error| error.to_string()).and_then(|surface| cut(&surface, &clip));
            let _ = sender.send(png.map(|png| json!({ "data": encode_base64(&png) }).to_string()));
        },
    );
    Ok(Pending { answer })
}

/// `clip` (in CSS pixels) of a document's snapshot, as a PNG. The snapshot's device scale maps CSS
/// pixels to its own, so the cut keeps a HiDPI screen's resolution.
#[cfg(target_os = "linux")]
fn cut(snapshot: &cairo::Surface, clip: &Clip) -> Result<Vec<u8>, String> {
    let error = |error: cairo::Error| format!("cairo: {error}");
    let (scale, _) = snapshot.device_scale();
    let pixels = |css: f64| ((css * scale).round() as i32).max(1);
    let image =
        cairo::ImageSurface::create(cairo::Format::ARgb32, pixels(clip.width), pixels(clip.height)).map_err(error)?;
    image.set_device_scale(scale, scale);
    {
        let context = cairo::Context::new(&image).map_err(error)?;
        context.set_source_surface(snapshot, -clip.x, -clip.y).map_err(error)?;
        context.paint().map_err(error)?;
    }
    let mut png = Vec::new();
    image.write_to_png(&mut png).map_err(|error| format!("cannot encode the PNG: {error}"))?;
    Ok(png)
}

/// Asks the WKWebView of the window whose runtime is current to render `clip`: a snapshot of the
/// view, or, where the box reaches past the window's viewport (which a snapshot leaves blank), a
/// PDF of the document cut to the box, drawn at the window's scale.
#[cfg(target_os = "macos")]
pub(crate) fn start(clip: &Clip) -> Result<Pending, Error> {
    use block2::RcBlock;
    use dioxus_desktop::wry::WebViewExtMacOS;
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSImage;
    use objc2_foundation::{NSData, NSError, NSPoint, NSRect, NSSize};
    use objc2_web_kit::{WKPDFConfiguration, WKSnapshotConfiguration};

    let desktop = dioxus_core::try_consume_context::<dioxus_desktop::DesktopContext>()
        .ok_or_else(|| Error::BadCommand("screenshot needs a Dioxus desktop window".into()))?;
    // The registry runs on the UI thread, which on macOS is the main thread.
    let main = MainThreadMarker::new().ok_or_else(|| Error::Script("screenshot ran off the main thread".into()))?;
    let webview = desktop.webview.webview();
    let zoom = unsafe { webview.pageZoom() };
    let rect = |[x, y, width, height]: [f64; 4]| NSRect::new(NSPoint::new(x, y), NSSize::new(width, height));

    // Each handler runs once, on the main thread, after `start` has returned: it hands its result
    // over, as Linux's does.
    let (sender, answer) = futures_channel::oneshot::channel();
    let sender = std::cell::Cell::new(Some(sender));
    let send = move |png: Result<Vec<u8>, String>| {
        if let Some(sender) = sender.take() {
            let _ = sender.send(png.map(|png| json!({ "data": encode_base64(&png) }).to_string()));
        }
    };
    // SAFETY (both handlers): WebKit passes a valid object, or null and an error, for the call.
    let failed = |error: *mut NSError| {
        unsafe { error.as_ref() }
            .map_or_else(|| "WebKit returned nothing".into(), |e| e.localizedDescription().to_string())
    };
    if clip.beyond {
        let scale = desktop.window.scale_factor();
        let [_, _, width, height] = document_rect(clip, zoom);
        let configuration = unsafe { WKPDFConfiguration::new(main) };
        unsafe { configuration.setRect(rect(document_rect(clip, zoom))) };
        let handler = RcBlock::new(move |pdf: *mut NSData, error: *mut NSError| {
            send(match unsafe { pdf.as_ref() } {
                Some(pdf) => rasterize(pdf, width, height, scale),
                None => Err(failed(error)),
            })
        });
        unsafe { webview.createPDFWithConfiguration_completionHandler(Some(&configuration), &handler) };
    } else {
        let configuration = unsafe { WKSnapshotConfiguration::new(main) };
        unsafe {
            configuration.setRect(rect(view_rect(clip, zoom)));
            configuration.setAfterScreenUpdates(true);
        }
        let handler = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
            send(match unsafe { image.as_ref() } {
                Some(image) => encode_png(image),
                None => Err(failed(error)),
            })
        });
        unsafe { webview.takeSnapshotWithConfiguration_completionHandler(Some(&configuration), &handler) };
    }
    Ok(Pending { answer })
}

/// A one-page PDF of `width` x `height` points, drawn at `scale` pixels a point, as a PNG.
#[cfg(target_os = "macos")]
fn rasterize(pdf: &objc2_foundation::NSData, width: f64, height: f64, scale: f64) -> Result<Vec<u8>, String> {
    use objc2::AnyThread;
    use objc2_app_kit::{NSBitmapImageRep, NSDeviceRGBColorSpace, NSGraphicsContext, NSPDFImageRep};
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let page = NSPDFImageRep::imageRepWithData(pdf).ok_or("WebKit's PDF is unreadable")?;
    let pixels = |points: f64| ((points * scale).round() as isize).max(1);
    let bitmap = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            pixels(width),
            pixels(height),
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            0,
            0,
        )
    }
    .ok_or("cannot make a bitmap for the PDF")?;
    // Points, so that drawing scales to the bitmap's pixels.
    bitmap.setSize(NSSize::new(width, height));
    let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&bitmap).ok_or("cannot draw into the bitmap")?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&context));
    let drawn = page.drawInRect(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)));
    context.flushGraphics();
    NSGraphicsContext::restoreGraphicsState_class();
    if !drawn {
        return Err("cannot draw WebKit's PDF".into());
    }
    encode_bitmap(&bitmap)
}

/// A snapshot as a PNG, at the pixel size of its bitmap: twice its size in points on a Retina screen.
#[cfg(target_os = "macos")]
fn encode_png(image: &objc2_app_kit::NSImage) -> Result<Vec<u8>, String> {
    use objc2_app_kit::NSBitmapImageRep;

    let tiff = image.TIFFRepresentation().ok_or("the snapshot has no bitmap")?;
    let bitmap = NSBitmapImageRep::imageRepWithData(&tiff).ok_or("the snapshot's bitmap is unreadable")?;
    encode_bitmap(&bitmap)
}

#[cfg(target_os = "macos")]
fn encode_bitmap(bitmap: &objc2_app_kit::NSBitmapImageRep) -> Result<Vec<u8>, String> {
    use objc2_app_kit::NSBitmapImageFileType;
    use objc2_foundation::NSDictionary;

    let png = unsafe { bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }
        .ok_or("cannot encode the PNG")?;
    Ok(png.to_vec())
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
        let clip = Clip::from_rect(
            &json!({ "x": 10.5, "y": 2000, "width": 300, "height": 40, "left": 10.5, "top": 500, "inside": false }),
        )
        .unwrap();
        let parameters = parameters(&clip);
        assert_eq!(parameters["clip"], json!({ "x": 10.5, "y": 2000.0, "width": 300.0, "height": 40.0, "scale": 1 }));
        assert_eq!(parameters["captureBeyondViewport"], true);
        let inside = Clip::from_rect(
            &json!({ "x": 0, "y": 0, "width": 300, "height": 40, "left": 0, "top": 0, "inside": true }),
        )
        .unwrap();
        assert_eq!(super::parameters(&inside)["captureBeyondViewport"], false);
        assert!(Clip::from_rect(&json!({ "x": 0, "y": 0, "width": 0, "height": 40, "left": 0, "top": 0 })).is_err());
        assert!(Clip::from_rect(&json!({ "x": 0, "y": 0, "width": 300, "height": 40 })).is_err());
    }

    #[test]
    fn webkit_takes_the_box_where_it_is_in_the_viewport() {
        // An element 1500 CSS pixels down a page scrolled by 1200: 300 down the viewport.
        let clip = Clip::from_rect(
            &json!({ "x": 8, "y": 1500, "width": 200, "height": 50.5, "left": 8, "top": 300, "inside": true }),
        )
        .unwrap();
        assert_eq!(view_rect(&clip, 1.0), [8.0, 300.0, 200.0, 50.5]);
        assert_eq!(view_rect(&clip, 1.5), [12.0, 450.0, 300.0, 75.75]);
        // Its PDF is cut from the whole document.
        assert_eq!(document_rect(&clip, 1.0), [8.0, 1500.0, 200.0, 50.5]);
        assert_eq!(document_rect(&clip, 1.5), [12.0, 2250.0, 300.0, 75.75]);
    }

    #[test]
    fn base64_round_trips() {
        for text in ["", "a", "ab", "abc", "abcd", "hello world"] {
            let encoded = encode_base64(text.as_bytes());
            assert_eq!(decode_base64(&encoded).as_deref(), Some(text.as_bytes()), "{encoded}");
        }
        assert_eq!(encode_base64(b"hello"), "aGVsbG8=");
        assert_eq!(decode_base64(PIXEL).map(|png| encode_base64(&png)).as_deref(), Some(PIXEL));
    }

    #[test]
    fn replies_carry_the_png_and_its_size() {
        let reply = reply(&json!({ "data": PIXEL }).to_string(), None).unwrap();
        assert_eq!((reply["width"].as_u64(), reply["height"].as_u64()), (Some(1), Some(1)));
        assert_eq!(reply["base64Data"], PIXEL);
        assert!(super::reply(r#"{"data":"aGVsbG8gd29ybGQgdGhpcyBpcyBub3QgYSBwbmc="}"#, None).is_err());
    }
}
