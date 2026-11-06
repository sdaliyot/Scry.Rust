//! The page a Dioxus app shows, driven through its webview: commands that run JavaScript in it.
//!
//! - `js <code>` runs `code` as the body of an async function and returns what it returns (as
//!   JSON). It can `await`, and a thrown error is reported as `script_error`.
//! - `click <selector>` clicks the first element that matches, through the DOM, so the app's own
//!   `onclick` handlers run as they would for the user.
//! - `type <selector> <text>` puts `text` into an input, textarea or select and fires `input` and
//!   `change`, so `oninput`/`onchange` handlers see it.
//! - `text [selector]` returns the visible text of an element, or of the whole page.
//!
//! A selector is a CSS selector (`#add`, `button.primary`, `li:nth-child(2) input`), or
//! `text=Label` for the innermost element whose visible text is `Label`, ignoring case and runs of
//! whitespace (a label CSS shows in capitals still matches as written in the code). One with spaces
//! is written as a JSON string: `click "nav a"`.
//!
//! Event handlers in Dioxus are closures inside the rendered tree, out of reach of the registry; the
//! webview is the one way in that every Dioxus renderer with a webview shares (`document::eval`).
//! These commands run on the UI thread like the rest, but asynchronously: the pump awaits the
//! webview's answer while the app keeps handling its own events.

use std::future::IntoFuture;
use std::time::Duration;

use futures_util::future::{Either, select};
use serde_json::Value;

use crate::Error;

/// Commands answered here rather than by the registry.
pub(crate) const VERBS: &[&str] = &["js", "click", "type", "text"];

/// How long the page may take to answer.
const TIMEOUT: Duration = crate::run_async::CALL_TIMEOUT;

/// Helpers every script gets: `__scry.find(selector)` and `__scry.describe(element)`.
const PRELUDE: &str = r#"
const __scry = {
  // Shown, and not parked beside the window (as overlays waiting to slide in are). An element
  // below the fold still counts: a click scrolls it into view.
  visible(el) {
    const style = getComputedStyle(el);
    if (style.visibility === "hidden" || style.opacity === "0") return false;
    return [...el.getClientRects()].some((rect) => rect.right > 0 && rect.left < window.innerWidth);
  },
  // The visible text under `root`: its innerText, without positioned elements that aren't visible.
  text(root) {
    const hidden = [...root.querySelectorAll("*")].filter((el) => {
      const position = getComputedStyle(el).position;
      return (position === "fixed" || position === "absolute") && !__scry.visible(el);
    });
    const saved = hidden.map((el) => el.style.display);
    hidden.forEach((el) => (el.style.display = "none"));
    try {
      return root.innerText;
    } finally {
      hidden.forEach((el, i) => (el.style.display = saved[i]));
    }
  },
  find(selector) {
    if (selector.startsWith("text=")) {
      // As shown: innerText applies CSS, so a label styled uppercase reads "TOTAL"; compare
      // without case, and with runs of whitespace as one space.
      const normal = (text) => (text ?? "").replace(/\s+/g, " ").trim().toLowerCase();
      const wanted = normal(selector.slice(5));
      const matches = [...document.body.querySelectorAll("*")]
        .filter((el) => normal(el.textContent).includes(wanted) && normal(el.innerText) === wanted && __scry.visible(el));
      return matches.find((el) => !matches.some((other) => other !== el && el.contains(other))) ?? null;
    }
    const all = [...document.querySelectorAll(selector)];
    return all.find(__scry.visible) ?? all[0] ?? null;
  },
  get(selector) {
    const el = __scry.find(selector);
    if (!el) throw new Error(`no element matches ${selector}`);
    return el;
  },
  describe(el) {
    if (el instanceof HTMLInputElement && (el.type === "checkbox" || el.type === "radio")) {
      return { tag: "input", id: el.id || null, checked: el.checked };
    }
    const field = el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el instanceof HTMLSelectElement;
    const text = ((field ? el.value : el.innerText) ?? "").trim();
    return { tag: el.tagName.toLowerCase(), id: el.id || null, text: text.length > 80 ? text.slice(0, 80) + "…" : text };
  },
};
"#;

/// The JavaScript for a command, or `None` when the command is not one of [`VERBS`].
pub(crate) fn script(line: &str) -> Option<Result<String, Error>> {
    let line = line.trim();
    let (verb, rest) = line.split_once(char::is_whitespace).map_or((line, ""), |(verb, rest)| (verb, rest.trim()));
    if !VERBS.contains(&verb) {
        return None;
    }
    Some(build(verb, rest))
}

fn build(verb: &str, rest: &str) -> Result<String, Error> {
    let body = match verb {
        "js" if rest.is_empty() => return Err(Error::BadCommand("js needs code to run".into())),
        "js" => return Ok(wrap(rest)),
        "click" => {
            let selector = whole_argument(rest, "a selector")?;
            format!(
                // Clicked on the next tick, after the answer is on its way: a click that makes the app
                // navigate can otherwise cost the answer (the webview drops it while handling the
                // click's event).
                "const el = __scry.get({selector}); el.scrollIntoView({{ block: \"nearest\" }}); \
                 const clicked = __scry.describe(el); setTimeout(() => el.click(), 0); return clicked;",
                selector = quote(&selector)
            )
        }
        "type" => {
            let (selector, text) = argument(rest, "a selector")?;
            let text = text_argument(text)?;
            format!(
                "const el = __scry.get({selector}); el.focus(); \
                 const proto = Object.getPrototypeOf(el); \
                 const setter = Object.getOwnPropertyDescriptor(proto, \"value\")?.set; \
                 if (!setter) throw new Error(`${{el.tagName.toLowerCase()}} has no value to type into`); \
                 setter.call(el, {text}); \
                 el.dispatchEvent(new Event(\"input\", {{ bubbles: true }})); \
                 el.dispatchEvent(new Event(\"change\", {{ bubbles: true }})); \
                 return __scry.describe(el);",
                selector = quote(&selector),
                text = quote(&text)
            )
        }
        "text" if rest.is_empty() => "return __scry.text(document.body);".to_owned(),
        "text" => {
            let selector = whole_argument(rest, "a selector")?;
            format!("return __scry.text(__scry.get({}));", quote(&selector))
        }
        _ => unreachable!("not one of VERBS"),
    };
    Ok(wrap(&body))
}

/// The marker a failed script returns its error under: the webview reports a thrown error to Rust
/// without its message, so the script catches it and returns it instead.
const ERROR_KEY: &str = "__scry_error";

fn wrap(body: &str) -> String {
    format!(
        "{PRELUDE}\ntry {{\n{body}\n}} catch (error) {{ return {{ {ERROR_KEY}: String(error?.message ?? error) }}; }}"
    )
}

/// The first argument of `rest` - a JSON string, or a word - and what follows it.
fn argument<'a>(rest: &'a str, what: &str) -> Result<(String, &'a str), Error> {
    if rest.is_empty() {
        return Err(Error::BadCommand(format!("expected {what}")));
    }
    if rest.starts_with('"') {
        let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<String>();
        let value = stream
            .next()
            .ok_or_else(|| Error::BadCommand(format!("expected {what}")))?
            .map_err(|error| Error::BadCommand(format!("{what}: {error}")))?;
        return Ok((value, rest[stream.byte_offset()..].trim_start()));
    }
    Ok(rest
        .split_once(char::is_whitespace)
        .map_or((rest.to_owned(), ""), |(word, rest)| (word.to_owned(), rest.trim_start())))
}

/// A command's only argument: a JSON string, or the rest of the line as it is, spaces and all
/// (`click li:nth-child(2) input`, `click text=Save changes`).
pub(crate) fn whole_argument(rest: &str, what: &str) -> Result<String, Error> {
    if rest.is_empty() {
        return Err(Error::BadCommand(format!("expected {what}")));
    }
    text_argument(rest)
}

/// The text to type: a JSON string, or the rest of the line as it is.
fn text_argument(rest: &str) -> Result<String, Error> {
    if rest.starts_with('"') {
        serde_json::from_str(rest).map_err(|error| Error::BadCommand(format!("the text: {error}")))
    } else {
        Ok(rest.to_owned())
    }
}

fn quote(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

/// Runs `script` in the app's webview and waits for its answer.
/// Sends `script` to the webview of the window whose runtime is current.
pub(crate) fn start(script: &str) -> dioxus_document::Eval {
    dioxus_document::eval(script)
}

/// Waits for a script's answer.
pub(crate) async fn finish(eval: dioxus_document::Eval) -> Result<Value, Error> {
    match within_timeout(eval.into_future()).await? {
        Ok(Value::Object(object)) if object.len() == 1 && object.contains_key(ERROR_KEY) => {
            Err(Error::Script(object[ERROR_KEY].as_str().unwrap_or_default().to_owned()))
        }
        Ok(value) => Ok(value),
        // What a page that went away while running the script - a click that closed its window -
        // reports.
        Err(dioxus_document::EvalError::Finished) => {
            Err(Error::Script("the page went away before it answered (its window closed?)".into()))
        }
        Err(error) => Err(Error::Script(error.to_string())),
    }
}

/// `future`'s output, or a timeout error if the page takes longer than [`TIMEOUT`] to answer.
pub(crate) async fn within_timeout<F: std::future::Future>(future: F) -> Result<F::Output, Error> {
    let (sender, timeout) = futures_channel::oneshot::channel::<()>();
    std::thread::spawn(move || {
        std::thread::sleep(TIMEOUT);
        let _ = sender.send(());
    });
    match select(std::pin::pin!(future), timeout).await {
        Either::Left((output, _)) => Ok(output),
        Either::Right(_) => Err(Error::Timeout(format!("the page did not answer within {}s", TIMEOUT.as_secs_f32()))),
    }
}

/// A script that returns the window's viewport as a box in page coordinates, like [`rect_script`].
pub(crate) fn viewport_script() -> String {
    wrap(
        "return { x: window.scrollX, y: window.scrollY, width: window.innerWidth, height: window.innerHeight,          inside: true };",
    )
}

/// A script that finds the element `selector` names, scrolls it into view within its scrolling
/// containers (as `click` does), and returns its box in page coordinates (CSS pixels), whether it
/// lies inside the window's viewport, and what it is: `{x, y, width, height, inside, element}`.
pub(crate) fn rect_script(selector: &str) -> String {
    wrap(&format!(
        "const el = __scry.get({}); el.scrollIntoView({{ block: \"nearest\" }}); \
         const r = el.getBoundingClientRect(); \
         return {{ x: r.left + window.scrollX, y: r.top + window.scrollY, width: r.width, height: r.height, \
         inside: r.top >= 0 && r.left >= 0 && r.bottom <= window.innerHeight && r.right <= window.innerWidth, \
         element: __scry.describe(el) }};",
        quote(selector)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_become_scripts_with_quoted_arguments() {
        assert!(script("get ui").is_none());
        let click = script("click text=Add").unwrap().unwrap();
        assert!(
            click.contains("__scry.get(\"text=Add\")") && click.contains("setTimeout(() => el.click(), 0)"),
            "{click}"
        );
        let typed = script("type \"li input.title\" Buy \"milk\"").unwrap().unwrap();
        assert!(typed.contains("__scry.get(\"li input.title\")"), "{typed}");
        assert!(typed.contains("setter.call(el, \"Buy \\\"milk\\\"\")"), "{typed}");
        let typed = script("type #draft \"two  spaces\"").unwrap().unwrap();
        assert!(typed.contains("setter.call(el, \"two  spaces\")"), "{typed}");
        assert!(script("text").unwrap().unwrap().contains("\nreturn __scry.text(document.body);\n} catch"));
        assert!(script("js return 1 + 1").unwrap().unwrap().contains("try {\nreturn 1 + 1\n} catch"));
        let click = script("click text=Save changes").unwrap().unwrap();
        assert!(click.contains("__scry.get(\"text=Save changes\")"), "{click}");
        let text = script("text li:nth-child(2) input").unwrap().unwrap();
        assert!(text.contains("__scry.get(\"li:nth-child(2) input\")"), "{text}");
        assert!(script("click").unwrap().is_err());
        assert!(script("js").unwrap().is_err());
        assert!(script("type \"unclosed").unwrap().is_err());
    }
}
