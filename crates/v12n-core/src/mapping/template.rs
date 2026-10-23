// SPDX-License-Identifier: Apache-2.0
//! `rr:template` strings split into literal text and column references.

use std::fmt;

use crate::Error;

/// A parsed `rr:template` (R2RML §7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    source: String,
    parts: Vec<TemplatePart>,
}

/// One piece of a template, with escapes already removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplatePart {
    Literal(String),
    Column(String),
}

impl Template {
    /// Parses a template string.
    ///
    /// R2RML §7.3: `{` and `}` that are not column delimiters, and `\`
    /// itself, must be escaped with `\`, both inside and outside column
    /// names. Any other escape is rejected rather than guessed at.
    pub fn parse(source: &str) -> Result<Self, Error> {
        let invalid = |reason: &str| Error::InvalidTemplate {
            template: source.to_owned(),
            reason: reason.to_owned(),
        };
        let mut parts = Vec::new();
        let mut current = String::new();
        let mut in_column = false;
        let mut chars = source.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some(escaped @ ('{' | '}' | '\\')) => current.push(escaped),
                    Some(_) => {
                        return Err(invalid(
                            "only \\{, \\} and \\\\ are valid escapes; write \\\\ for a backslash",
                        ));
                    }
                    None => return Err(invalid("ends with a lone backslash")),
                },
                '{' if in_column => return Err(invalid("nested '{' in a column reference")),
                '{' => {
                    if !current.is_empty() {
                        parts.push(TemplatePart::Literal(std::mem::take(&mut current)));
                    }
                    in_column = true;
                }
                '}' if in_column => {
                    if current.is_empty() {
                        return Err(invalid("empty column reference '{}'"));
                    }
                    parts.push(TemplatePart::Column(std::mem::take(&mut current)));
                    in_column = false;
                }
                '}' => return Err(invalid("unmatched '}'; write \\} for a literal brace")),
                _ => current.push(c),
            }
        }
        if in_column {
            return Err(invalid("unclosed '{'"));
        }
        if !current.is_empty() {
            parts.push(TemplatePart::Literal(current));
        }
        Ok(Self {
            source: source.to_owned(),
            parts,
        })
    }

    /// The template exactly as written in the mapping.
    pub fn as_str(&self) -> &str {
        &self.source
    }

    pub fn parts(&self) -> &[TemplatePart] {
        &self.parts
    }

    /// Column names in order of appearance, unescaped.
    pub fn columns(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().filter_map(|p| match p {
            TemplatePart::Column(c) => Some(c.as_str()),
            TemplatePart::Literal(_) => None,
        })
    }

    /// Whether the strings this template renders as an IRI template start
    /// with a scheme ([`has_scheme`](super::base_iri::has_scheme)).
    ///
    /// Column values are IRI-safe encoded (R2RML §7.3), so they never
    /// contain `:`; whether a scheme exists is decided by the literal text
    /// before the first `:`, unless a column comes before it.
    pub(crate) fn iri_kind(&self) -> IriKind {
        let mut column_seen = false;
        let mut first = true;
        for part in &self.parts {
            let text = match part {
                TemplatePart::Column(_) => {
                    column_seen = true;
                    continue;
                }
                TemplatePart::Literal(text) => text,
            };
            for c in text.chars() {
                if c == ':' {
                    return match (first, column_seen) {
                        (_, true) => IriKind::Mixed,
                        (true, false) => IriKind::Relative,
                        (false, false) => IriKind::Absolute,
                    };
                }
                let scheme_char = if first && !column_seen {
                    c.is_ascii_alphabetic()
                } else {
                    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
                };
                if !scheme_char {
                    return IriKind::Relative;
                }
                first = false;
            }
        }
        IriKind::Relative
    }

    /// The template with `prefix` in front of everything it renders.
    pub(crate) fn with_prefix(&self, prefix: &str) -> Self {
        let mut parts = self.parts.clone();
        match parts.first_mut() {
            Some(TemplatePart::Literal(text)) => text.insert_str(0, prefix),
            _ => parts.insert(0, TemplatePart::Literal(prefix.to_owned())),
        }
        let mut source = String::with_capacity(prefix.len() + self.source.len());
        for c in prefix.chars() {
            if matches!(c, '{' | '}' | '\\') {
                source.push('\\');
            }
            source.push(c);
        }
        source.push_str(&self.source);
        Self { source, parts }
    }
}

/// Whether the strings of an IRI template have a scheme, which tells an
/// absolute IRI from a relative one (R2RML §11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IriKind {
    /// Every string has a scheme.
    Absolute,
    /// No string has one: every string is relative.
    Relative,
    /// It depends on the column values, as in `{scheme}:{rest}`.
    Mixed,
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

/// `ucschar` from RFC 3987: the non-ASCII part of `iunreserved`, as
/// inclusive code point ranges.
pub(crate) const UCSCHAR: &[(u32, u32)] = &[
    (0xA0, 0xD7FF),
    (0xF900, 0xFDCF),
    (0xFDF0, 0xFFEF),
    (0x10000, 0x1FFFD),
    (0x20000, 0x2FFFD),
    (0x30000, 0x3FFFD),
    (0x40000, 0x4FFFD),
    (0x50000, 0x5FFFD),
    (0x60000, 0x6FFFD),
    (0x70000, 0x7FFFD),
    (0x80000, 0x8FFFD),
    (0x90000, 0x9FFFD),
    (0xA0000, 0xAFFFD),
    (0xB0000, 0xBFFFD),
    (0xC0000, 0xCFFFD),
    (0xD0000, 0xDFFFD),
    (0xE1000, 0xEFFFD),
];

/// `iunreserved` from RFC 3987, which R2RML's IRI-safe version keeps.
pub(crate) fn is_iunreserved(c: char) -> bool {
    let code = c as u32;
    c.is_ascii_alphanumeric()
        || matches!(c, '-' | '.' | '_' | '~')
        || UCSCHAR
            .iter()
            .any(|&(from, to)| (from..=to).contains(&code))
}

/// R2RML §7.3 IRI-safe version of a string: everything except
/// `iunreserved` is percent-encoded as UTF-8.
pub(crate) fn iri_safe(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if is_iunreserved(c) {
            out.push(c);
        } else {
            let mut buf = [0; 4];
            for byte in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &str) -> TemplatePart {
        TemplatePart::Literal(s.to_owned())
    }

    fn col(s: &str) -> TemplatePart {
        TemplatePart::Column(s.to_owned())
    }

    #[test]
    fn parse_iri_template_splits_literal_and_column() {
        let t = Template::parse("http://example.org/customer/{id}").unwrap();
        assert_eq!(t.parts(), [lit("http://example.org/customer/"), col("id")]);
        assert_eq!(t.columns().collect::<Vec<_>>(), ["id"]);
        assert_eq!(t.as_str(), "http://example.org/customer/{id}");
    }

    #[test]
    fn parse_adjacent_columns_keeps_order() {
        let t = Template::parse("{a}{b}-{c}").unwrap();
        assert_eq!(t.parts(), [col("a"), col("b"), lit("-"), col("c")]);
        assert_eq!(t.columns().collect::<Vec<_>>(), ["a", "b", "c"]);
    }

    #[test]
    fn parse_escapes_outside_and_inside_columns_unescapes() {
        let t = Template::parse(r"x\{y\}\\{we\{ird\}\\col}z").unwrap();
        assert_eq!(t.parts(), [lit(r"x{y}\"), col(r"we{ird}\col"), lit("z")]);
    }

    #[test]
    fn parse_column_with_spaces_and_quotes_keeps_name_verbatim() {
        let t = Template::parse(r#"http://ex/{"Order Id"}"#).unwrap();
        assert_eq!(t.columns().collect::<Vec<_>>(), [r#""Order Id""#]);
    }

    #[test]
    fn iri_safe_keeps_only_iunreserved() {
        assert_eq!(iri_safe("a-b.c_d~e"), "a-b.c_d~e");
        assert_eq!(iri_safe("a b/c%"), "a%20b%2Fc%25");
        assert_eq!(iri_safe("한글"), "한글");
        // Private use and the start of plane 14 are not `ucschar`.
        assert_eq!(iri_safe("\u{E000}"), "%EE%80%80");
        assert_eq!(iri_safe("\u{E0001}"), "%F3%A0%80%81");
        assert_eq!(iri_safe("\u{E1000}"), "\u{E1000}");
        assert_eq!(iri_safe("\u{1FFFE}"), "%F0%9F%BF%BE");
    }

    #[test]
    fn iri_kind_reads_the_scheme_off_the_literal_text() {
        let kind = |source: &str| Template::parse(source).unwrap().iri_kind();
        for absolute in ["http://ex/{id}", "urn:x:{id}", "a+b.c-d:{id}", "x:"] {
            assert_eq!(kind(absolute), IriKind::Absolute, "{absolute}");
        }
        for relative in [
            "{id}",
            "{\"Name\"}",
            "person/{id}",
            "{a}/{b}:{c}",
            "{a}_x:{b}",
            ":{id}",
            "1x:{id}",
            "a b:{id}",
            "",
        ] {
            assert_eq!(kind(relative), IriKind::Relative, "{relative}");
        }
        for mixed in ["{a}:{b}", "x{a}:y", "{a}1+b.c-d:{e}"] {
            assert_eq!(kind(mixed), IriKind::Mixed, "{mixed}");
        }
    }

    #[test]
    fn with_prefix_prepends_to_parts_and_source() {
        let t = Template::parse("{\"Name\"}").unwrap();
        let prefixed = t.with_prefix("http://example.com/base/");
        assert_eq!(
            prefixed.parts(),
            [lit("http://example.com/base/"), col("\"Name\"")]
        );
        assert_eq!(prefixed.as_str(), "http://example.com/base/{\"Name\"}");
        let t = Template::parse("person/{id}").unwrap();
        let prefixed = t.with_prefix("http://ex/");
        assert_eq!(prefixed.parts(), [lit("http://ex/person/"), col("id")]);
        assert_eq!(Template::parse(prefixed.as_str()).unwrap(), prefixed);
        // Braces cannot be in an IRI, but the source stays a valid template.
        let odd = Template::parse("{id}").unwrap().with_prefix("x{\\}");
        assert_eq!(Template::parse(odd.as_str()).unwrap(), odd);
    }

    #[test]
    fn parse_malformed_braces_errors() {
        for bad in [
            "http://ex/{id",
            "http://ex/id}",
            "http://ex/{}",
            "http://ex/{a{b}}",
            r"http://ex/{id}\",
            r"http://ex/\n{id}",
        ] {
            let err = Template::parse(bad).unwrap_err();
            assert!(
                matches!(err, Error::InvalidTemplate { ref template, .. } if template == bad),
                "{bad}: {err}"
            );
        }
    }
}
