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
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

/// `iunreserved` from RFC 3987, which R2RML's IRI-safe version keeps.
pub(crate) fn is_iunreserved(c: char) -> bool {
    let code = c as u32;
    c.is_ascii_alphanumeric()
        || matches!(c, '-' | '.' | '_' | '~')
        || matches!(code, 0xA0..=0xD7FF | 0xF900..=0xFDCF | 0xFDF0..=0xFFEF)
        || ((0x10000..=0xEFFFD).contains(&code) && code & 0xFFFF <= 0xFFFD)
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
