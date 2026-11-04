use std::fmt;

use crate::Error;

/// One step of a path: a named member (`.qty`, or `.0` for a tuple field) or an index (`[2]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Field(String),
    Index(usize),
}

impl Segment {
    pub fn field(&self) -> Option<&str> {
        match self {
            Segment::Field(name) => Some(name),
            Segment::Index(_) => None,
        }
    }

    /// `[2]` and `.2` both index a sequence.
    pub fn index(&self) -> Option<usize> {
        match self {
            Segment::Index(index) => Some(*index),
            Segment::Field(name) => name.parse().ok(),
        }
    }
}

impl fmt::Display for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Segment::Field(name) => f.write_str(name),
            Segment::Index(index) => write!(f, "[{index}]"),
        }
    }
}

/// Splits `root.member[3].other` into the root name and the segments after it.
pub(crate) fn parse(path: &str) -> Result<(String, Vec<Segment>), Error> {
    let path = path.trim();
    let root_end = path.find(['.', '[']).unwrap_or(path.len());
    let root = &path[..root_end];
    if root.is_empty() {
        return Err(Error::BadPath(format!("'{path}' does not start with a root name")));
    }

    let mut segments = Vec::new();
    let mut rest = &path[root_end..];
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            let end = after.find(['.', '[']).unwrap_or(after.len());
            let name = &after[..end];
            if name.is_empty() {
                return Err(Error::BadPath(format!("empty member name in '{path}'")));
            }
            segments.push(Segment::Field(name.to_owned()));
            rest = &after[end..];
        } else if let Some(after) = rest.strip_prefix('[') {
            let end = after.find(']').ok_or_else(|| Error::BadPath(format!("unclosed '[' in '{path}'")))?;
            let index = after[..end]
                .trim()
                .parse()
                .map_err(|_| Error::BadPath(format!("'{}' is not an index in '{path}'", &after[..end])))?;
            segments.push(Segment::Index(index));
            rest = &after[end + 1..];
        } else {
            return Err(Error::BadPath(format!("unexpected '{rest}' in '{path}'")));
        }
    }
    Ok((root.to_owned(), segments))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_members_and_indexes() {
        let (root, segments) = parse("app.items[2].qty").unwrap();
        assert_eq!(root, "app");
        assert_eq!(
            segments,
            vec![Segment::Field("items".into()), Segment::Index(2), Segment::Field("qty".into())]
        );
    }

    #[test]
    fn rejects_malformed_paths() {
        assert!(parse(".x").is_err());
        assert!(parse("a..b").is_err());
        assert!(parse("a[x]").is_err());
        assert!(parse("a[1").is_err());
    }
}
