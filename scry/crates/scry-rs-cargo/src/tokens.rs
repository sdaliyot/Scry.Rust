//! Rewrites that work on raw tokens rather than the syntax tree: inside `macro_rules!` templates,
//! which syn does not parse, and in `include!`-family calls, which may sit inside any macro.

use proc_macro2::{Delimiter, Group, Spacing, TokenStream, TokenTree};

use crate::edits::{Edits, Source};

/// Adds the derive to each `struct` and `enum` a `macro_rules!` template defines. Returns how many.
pub fn macro_templates(tokens: TokenStream, source: &Source, edits: &mut Edits) -> usize {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    let mut count = 0;
    // Rules are `(matcher) => {template}`; only templates are touched, never matchers.
    for window in tokens.windows(4) {
        if let [TokenTree::Group(_), TokenTree::Punct(eq), TokenTree::Punct(gt), TokenTree::Group(template)] = window
            && eq.as_char() == '='
            && eq.spacing() == Spacing::Joint
            && gt.as_char() == '>'
        {
            count += template_items(template.stream(), source, edits);
        }
    }
    count
}

fn template_items(stream: TokenStream, source: &Source, edits: &mut Edits) -> usize {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    let mut count = 0;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Group(group) => count += template_items(group.stream(), source, edits),
            TokenTree::Ident(ident) if ident == "struct" || ident == "enum" => {
                let start = item_start(&tokens, index);
                let attributes = attributes_before(&tokens, start);
                if !attributes.contains("Reflect") && !attributes.contains("packed") {
                    edits.insert(source.start(tokens[start].span()), "#[derive(::scry::Reflect)] ");
                    count += 1;
                }
            }
            _ => {}
        }
    }
    count
}

/// Steps back from the `struct` keyword over a visibility: `pub`, `pub(crate)`, or `$vis`.
fn item_start(tokens: &[TokenTree], keyword: usize) -> usize {
    let two_back = keyword >= 2
        && ((is_punct(&tokens[keyword - 2], '$') && matches!(tokens[keyword - 1], TokenTree::Ident(_)))
            || (is_ident(&tokens[keyword - 2], "pub") && is_group(&tokens[keyword - 1], Delimiter::Parenthesis)));
    if two_back {
        keyword - 2
    } else if keyword >= 1 && is_ident(&tokens[keyword - 1], "pub") {
        keyword - 1
    } else {
        keyword
    }
}

/// The text of the `#[...]` attributes right before an item.
fn attributes_before(tokens: &[TokenTree], start: usize) -> String {
    let mut text = String::new();
    let mut index = start;
    while index >= 2 && is_punct(&tokens[index - 2], '#') && is_group(&tokens[index - 1], Delimiter::Bracket) {
        text.push_str(&tokens[index - 1].to_string());
        index -= 2;
    }
    text
}

fn is_punct(token: &TokenTree, c: char) -> bool {
    matches!(token, TokenTree::Punct(punct) if punct.as_char() == c)
}

fn is_ident(token: &TokenTree, name: &str) -> bool {
    matches!(token, TokenTree::Ident(ident) if ident == name)
}

fn is_group(token: &TokenTree, delimiter: Delimiter) -> bool {
    matches!(token, TokenTree::Group(group) if group.delimiter() == delimiter)
}

/// An `include!`-family call that has to change once the file moves to the rewritten tree.
pub enum Include {
    /// `include_str!("../data.txt")` and friends with a relative path: the path is made absolute,
    /// relative to the original file.
    Relative { start: usize, end: usize, path: String },
    /// `include!(concat!(env!("OUT_DIR"), "/x.rs"))`, or `include_proto!("x")`: code a build
    /// script generated, rewritten like the crate's own files. Replace `start..end` with an
    /// `include!` of the rewritten file.
    BuildOutput { start: usize, end: usize, file: String },
}

pub fn includes(source: &Source) -> Vec<Include> {
    let text = source.text.strip_prefix('\u{feff}').unwrap_or(&source.text);
    let Ok(stream) = text.parse::<TokenStream>() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    scan_includes(stream, source, &mut found);
    found
}

fn scan_includes(stream: TokenStream, source: &Source, found: &mut Vec<Include>) {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    for (index, token) in tokens.iter().enumerate() {
        if let TokenTree::Group(group) = token {
            scan_includes(group.stream(), source, found);
            continue;
        }
        let TokenTree::Ident(ident) = token else { continue };
        let name = ident.to_string();
        if !matches!(name.as_str(), "include" | "include_str" | "include_bytes" | "include_proto") {
            continue;
        }
        let (Some(bang), Some(TokenTree::Group(group))) = (tokens.get(index + 1), tokens.get(index + 2)) else {
            continue;
        };
        if !is_punct(bang, '!') {
            continue;
        }
        let args: Vec<TokenTree> = group.stream().into_iter().collect();
        let args = match args.as_slice() {
            [rest @ .., last] if is_punct(last, ',') => rest,
            all => all,
        };
        match (name.as_str(), args) {
            ("include_proto", [TokenTree::Literal(literal)]) => {
                if let Some(package) = string_value(literal) {
                    let start = path_start(&tokens, index);
                    found.push(Include::BuildOutput {
                        start: source.start(tokens[start].span()),
                        end: source.end(group.span_close()),
                        file: format!("{package}.rs"),
                    });
                }
            }
            ("include_proto", _) => {}
            (_, [TokenTree::Literal(literal)]) => {
                if let Some(path) = string_value(literal)
                    && std::path::Path::new(&path).is_relative()
                {
                    found.push(Include::Relative { start: source.start(literal.span()), end: source.end(literal.span()), path });
                }
            }
            ("include", [TokenTree::Ident(concat), TokenTree::Punct(bang), TokenTree::Group(parts)])
                if concat == "concat" && bang.as_char() == '!' =>
            {
                if let Some(file) = out_dir_file(parts) {
                    found.push(Include::BuildOutput {
                        start: source.start(tokens[index].span()),
                        end: source.end(group.span_close()),
                        file,
                    });
                }
            }
            _ => {}
        }
    }
}

/// The file in `concat!(env!("OUT_DIR"), "/name.rs")`.
fn out_dir_file(parts: &Group) -> Option<String> {
    let parts: Vec<TokenTree> = parts.stream().into_iter().collect();
    match parts.as_slice() {
        [TokenTree::Ident(env), TokenTree::Punct(bang), TokenTree::Group(var), TokenTree::Punct(comma), TokenTree::Literal(file)]
            if env == "env" && bang.as_char() == '!' && comma.as_char() == ',' =>
        {
            let var: Vec<TokenTree> = var.stream().into_iter().collect();
            match var.as_slice() {
                [TokenTree::Literal(name)] if string_value(name).as_deref() == Some("OUT_DIR") => {
                    string_value(file).map(|file| file.trim_start_matches(['/', '\\']).to_owned())
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Steps back over a path such as `tonic::include_proto` or `::tonic::include_proto`.
fn path_start(tokens: &[TokenTree], mut index: usize) -> usize {
    while index >= 3 && is_punct(&tokens[index - 1], ':') && is_punct(&tokens[index - 2], ':') && matches!(tokens[index - 3], TokenTree::Ident(_)) {
        index -= 3;
    }
    if index >= 2 && is_punct(&tokens[index - 1], ':') && is_punct(&tokens[index - 2], ':') {
        index -= 2;
    }
    index
}

fn string_value(literal: &proc_macro2::Literal) -> Option<String> {
    syn::parse_str::<syn::LitStr>(&literal.to_string()).ok().map(|literal| literal.value())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_templates(text: &str) -> String {
        let source = Source::new(text.to_owned());
        let file = syn::parse_file(text).unwrap();
        let mut edits = Edits::default();
        for item in &file.items {
            if let syn::Item::Macro(item) = item {
                macro_templates(item.mac.tokens.clone(), &source, &mut edits);
            }
        }
        edits.apply(text)
    }

    #[test]
    fn derives_types_in_macro_templates_only() {
        let text = "macro_rules! id {\n    (struct $name:ident) => { $vis struct $name(u64); pub(crate) enum E { A } };\n}";
        let out = apply_templates(text);
        assert!(out.contains("(struct $name:ident)"), "{out}");
        assert!(out.contains("#[derive(::scry::Reflect)] $vis struct $name"), "{out}");
        assert!(out.contains("#[derive(::scry::Reflect)] pub(crate) enum E"), "{out}");
        assert_eq!(out.lines().count(), text.lines().count());
    }

    #[test]
    fn finds_includes() {
        let text = r#"const A: &str = include_str!("../a.txt");
include!(concat!(env!("OUT_DIR"), "/gen.rs"));
pub mod pb { tonic::include_proto!("hello"); }
const B: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/b.txt"));"#;
        let source = Source::new(text.to_owned());
        let found = includes(&source);
        assert_eq!(found.len(), 3);
        assert!(matches!(&found[0], Include::Relative { path, .. } if path == "../a.txt"));
        assert!(matches!(&found[1], Include::BuildOutput { file, start, .. } if file == "gen.rs" && &text[*start..*start + 8] == "include!"));
        assert!(matches!(&found[2], Include::BuildOutput { file, start, .. } if file == "hello.rs" && text[*start..].starts_with("tonic::")));
    }
}
