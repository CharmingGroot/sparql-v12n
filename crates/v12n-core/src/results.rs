// SPDX-License-Identifier: Apache-2.0
//! SPARQL 1.1 Query Results JSON, XML, CSV and TSV Formats, written one
//! solution at a time.

use std::io::{self, Write};

use oxrdf::{Literal, Term};

use crate::solution::Solution;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
const XSD_DOUBLE: &str = "http://www.w3.org/2001/XMLSchema#double";

/// A SPARQL 1.1 query results format for `SELECT` results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultsFormat {
    /// SPARQL 1.1 Query Results JSON Format.
    Json,
    /// SPARQL Query Results XML Format (Second Edition).
    Xml,
    /// SPARQL 1.1 Query Results CSV Format: lexical forms only.
    Csv,
    /// SPARQL 1.1 Query Results TSV Format: terms in SPARQL syntax.
    Tsv,
}

impl ResultsFormat {
    /// Every format, in the order a server prefers them when a client
    /// accepts several equally.
    pub const ALL: [Self; 4] = [Self::Json, Self::Xml, Self::Csv, Self::Tsv];

    /// The media type without parameters, as an `Accept` header names it.
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Json => "application/sparql-results+json",
            Self::Xml => "application/sparql-results+xml",
            Self::Csv => "text/csv",
            Self::Tsv => "text/tab-separated-values",
        }
    }

    /// The `Content-Type` of a response. The `text/*` types default to
    /// US-ASCII, so they name UTF-8 (SPARQL 1.1 Query Results CSV and TSV
    /// §1); JSON and XML are UTF-8 without a parameter.
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Csv => "text/csv; charset=utf-8",
            Self::Tsv => "text/tab-separated-values; charset=utf-8",
            other => other.media_type(),
        }
    }
}

/// Streams `SELECT` results in any [`ResultsFormat`], with the same
/// `start`, `write`, `finish` steps as each format's own writer.
pub enum ResultsWriter<W: Write> {
    Json(JsonResultsWriter<W>),
    Xml(XmlResultsWriter<W>),
    Csv(CsvResultsWriter<W>),
    Tsv(TsvResultsWriter<W>),
}

impl<W: Write> ResultsWriter<W> {
    /// Writes what comes before the first solution.
    pub fn start(format: ResultsFormat, out: W, variables: &[String]) -> io::Result<Self> {
        Ok(match format {
            ResultsFormat::Json => Self::Json(JsonResultsWriter::start(out, variables)?),
            ResultsFormat::Xml => Self::Xml(XmlResultsWriter::start(out, variables)?),
            ResultsFormat::Csv => Self::Csv(CsvResultsWriter::start(out, variables)?),
            ResultsFormat::Tsv => Self::Tsv(TsvResultsWriter::start(out, variables)?),
        })
    }

    /// Writes one solution.
    pub fn write(&mut self, solution: &Solution) -> io::Result<()> {
        match self {
            Self::Json(w) => w.write(solution),
            Self::Xml(w) => w.write(solution),
            Self::Csv(w) => w.write(solution),
            Self::Tsv(w) => w.write(solution),
        }
    }

    /// The underlying writer, e.g. to drain a buffer between solutions.
    pub fn get_mut(&mut self) -> &mut W {
        match self {
            Self::Json(w) => w.get_mut(),
            Self::Xml(w) => w.get_mut(),
            Self::Csv(w) => w.get_mut(),
            Self::Tsv(w) => w.get_mut(),
        }
    }

    /// Closes the document, if the format has an end, and returns the
    /// writer, unflushed.
    pub fn finish(self) -> io::Result<W> {
        match self {
            Self::Json(w) => w.finish(),
            Self::Xml(w) => w.finish(),
            Self::Csv(w) => w.finish(),
            Self::Tsv(w) => w.finish(),
        }
    }
}

/// Streams `SELECT` results as SPARQL 1.1 Query Results JSON: the head goes
/// out on [`start`](Self::start), each solution on
/// [`write`](Self::write), and the closing brackets on
/// [`finish`](Self::finish). Nothing is buffered beyond what `W` buffers.
pub struct JsonResultsWriter<W: Write> {
    out: W,
    variables: Vec<String>,
    written: u64,
}

impl<W: Write> JsonResultsWriter<W> {
    /// Writes the head with the projected variable names.
    pub fn start(mut out: W, variables: &[String]) -> io::Result<Self> {
        out.write_all(b"{\"head\":{\"vars\":")?;
        serde_json::to_writer(&mut out, variables)?;
        out.write_all(b"},\"results\":{\"bindings\":[")?;
        Ok(Self {
            out,
            variables: variables.to_vec(),
            written: 0,
        })
    }

    /// Writes one solution. Unbound variables are left out of the binding
    /// (SPARQL 1.1 Query Results JSON §3.2.2).
    pub fn write(&mut self, solution: &Solution) -> io::Result<()> {
        if self.written > 0 {
            self.out.write_all(b",")?;
        }
        self.out.write_all(b"\n{")?;
        let mut first = true;
        for (variable, value) in self.variables.iter().zip(solution.values()) {
            let Some(term) = value else { continue };
            if !first {
                self.out.write_all(b",")?;
            }
            first = false;
            serde_json::to_writer(&mut self.out, variable)?;
            self.out.write_all(b":")?;
            write_term(&mut self.out, term)?;
        }
        self.out.write_all(b"}")?;
        self.written += 1;
        Ok(())
    }

    /// The underlying writer, e.g. to drain a buffer between solutions.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.out
    }

    /// Closes the document and returns the writer, unflushed.
    pub fn finish(mut self) -> io::Result<W> {
        self.out.write_all(b"\n]}}\n")?;
        Ok(self.out)
    }
}

/// Streams `SELECT` results as SPARQL 1.1 Query Results CSV: a header of
/// variable names, then one record per solution. The format keeps only the
/// lexical form of each term (no datatype or language tag) and writes an
/// unbound variable like an empty string (SPARQL 1.1 Query Results CSV §2).
pub struct CsvResultsWriter<W: Write> {
    out: W,
    fields: Vec<String>,
}

impl<W: Write> CsvResultsWriter<W> {
    /// Writes the header record: the variable names without `?`.
    pub fn start(mut out: W, variables: &[String]) -> io::Result<Self> {
        write_csv_record(&mut out, variables)?;
        Ok(Self {
            out,
            fields: Vec::with_capacity(variables.len()),
        })
    }

    /// Writes one solution as one record.
    pub fn write(&mut self, solution: &Solution) -> io::Result<()> {
        self.fields.clear();
        for value in solution.values() {
            self.fields.push(match value {
                None => String::new(),
                Some(Term::NamedNode(n)) => n.as_str().to_owned(),
                Some(Term::BlankNode(b)) => format!("_:{}", b.as_str()),
                Some(Term::Literal(l)) => l.value().to_owned(),
                #[allow(unreachable_patterns)]
                Some(_) => return Err(quoted_triple()),
            });
        }
        write_csv_record(&mut self.out, &self.fields)
    }

    /// The underlying writer, e.g. to drain a buffer between solutions.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.out
    }

    /// Returns the writer, unflushed. CSV has no closing part.
    pub fn finish(self) -> io::Result<W> {
        Ok(self.out)
    }
}

/// Writes one CSV record ending in CRLF, quoting a field that holds a
/// comma, a double quote, CR or LF (RFC 4180 §2, as SPARQL 1.1 Query
/// Results CSV §2 requires).
pub fn write_csv_record<S: AsRef<str>>(out: &mut impl Write, fields: &[S]) -> io::Result<()> {
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            out.write_all(b",")?;
        }
        let field = field.as_ref();
        // A record of one empty field would be a blank line, which many
        // readers skip; quoting it keeps the record.
        if field.contains([',', '"', '\r', '\n']) || (fields.len() == 1 && field.is_empty()) {
            out.write_all(b"\"")?;
            out.write_all(field.replace('"', "\"\"").as_bytes())?;
            out.write_all(b"\"")?;
        } else {
            out.write_all(field.as_bytes())?;
        }
    }
    out.write_all(b"\r\n")
}

fn write_term(out: &mut impl Write, term: &Term) -> io::Result<()> {
    let (kind, value, extra) = match term {
        Term::NamedNode(n) => ("uri", n.as_str(), None),
        Term::BlankNode(b) => ("bnode", b.as_str(), None),
        Term::Literal(l) => {
            let extra = match l.language() {
                Some(language) => Some(("xml:lang", language)),
                None if l.datatype().as_str() == XSD_STRING => None,
                None => Some(("datatype", l.datatype().as_str())),
            };
            ("literal", l.value(), extra)
        }
        #[allow(unreachable_patterns)]
        _ => return Err(quoted_triple()),
    };
    out.write_all(b"{\"type\":\"")?;
    out.write_all(kind.as_bytes())?;
    out.write_all(b"\",\"value\":")?;
    serde_json::to_writer(&mut *out, value)?;
    if let Some((key, value)) = extra {
        out.write_all(b",\"")?;
        out.write_all(key.as_bytes())?;
        out.write_all(b"\":")?;
        serde_json::to_writer(&mut *out, value)?;
    }
    out.write_all(b"}")
}

/// Quoted triples exist only with oxrdf's `rdf-12` feature, and no
/// SPARQL 1.1 results format can hold them.
fn quoted_triple() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "quoted triples cannot be written as SPARQL 1.1 results",
    )
}

/// Streams `SELECT` results in the SPARQL Query Results XML Format: the
/// head and the opening `<results>` on [`start`](Self::start), one
/// `<result>` per line on [`write`](Self::write), and the closing tags on
/// [`finish`](Self::finish).
pub struct XmlResultsWriter<W: Write> {
    out: W,
    variables: Vec<String>,
}

impl<W: Write> XmlResultsWriter<W> {
    /// Writes the XML declaration, the head with the variable names and
    /// the opening `<results>`.
    pub fn start(mut out: W, variables: &[String]) -> io::Result<Self> {
        out.write_all(
            b"<?xml version=\"1.0\"?>\n<sparql xmlns=\"http://www.w3.org/2005/sparql-results#\"><head>",
        )?;
        for variable in variables {
            out.write_all(b"<variable name=\"")?;
            write_xml_escaped(&mut out, variable)?;
            out.write_all(b"\"/>")?;
        }
        out.write_all(b"</head><results>")?;
        Ok(Self {
            out,
            variables: variables.to_vec(),
        })
    }

    /// Writes one solution. An unbound variable has no `<binding>`
    /// (SPARQL Query Results XML Format §2.3.1).
    pub fn write(&mut self, solution: &Solution) -> io::Result<()> {
        self.out.write_all(b"\n<result>")?;
        for (variable, value) in self.variables.iter().zip(solution.values()) {
            let Some(term) = value else { continue };
            self.out.write_all(b"<binding name=\"")?;
            write_xml_escaped(&mut self.out, variable)?;
            self.out.write_all(b"\">")?;
            match term {
                Term::NamedNode(n) => {
                    self.out.write_all(b"<uri>")?;
                    write_xml_escaped(&mut self.out, n.as_str())?;
                    self.out.write_all(b"</uri>")?;
                }
                Term::BlankNode(b) => {
                    self.out.write_all(b"<bnode>")?;
                    write_xml_escaped(&mut self.out, b.as_str())?;
                    self.out.write_all(b"</bnode>")?;
                }
                Term::Literal(l) => {
                    self.out.write_all(b"<literal")?;
                    if let Some(language) = l.language() {
                        self.out.write_all(b" xml:lang=\"")?;
                        write_xml_escaped(&mut self.out, language)?;
                        self.out.write_all(b"\"")?;
                    } else if l.datatype().as_str() != XSD_STRING {
                        self.out.write_all(b" datatype=\"")?;
                        write_xml_escaped(&mut self.out, l.datatype().as_str())?;
                        self.out.write_all(b"\"")?;
                    }
                    self.out.write_all(b">")?;
                    write_xml_escaped(&mut self.out, l.value())?;
                    self.out.write_all(b"</literal>")?;
                }
                #[allow(unreachable_patterns)]
                _ => return Err(quoted_triple()),
            }
            self.out.write_all(b"</binding>")?;
        }
        self.out.write_all(b"</result>")
    }

    /// The underlying writer, e.g. to drain a buffer between solutions.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.out
    }

    /// Closes the document and returns the writer, unflushed.
    pub fn finish(mut self) -> io::Result<W> {
        self.out.write_all(b"\n</results></sparql>\n")?;
        Ok(self.out)
    }
}

/// Writes `text` for element content or a double-quoted attribute value.
/// Escaping CR, tab and line feed keeps them through XML end-of-line and
/// attribute-value normalization (XML 1.0 §2.11, §3.3.3). A character
/// that XML 1.0 cannot hold even as a reference (§2.2), such as U+0001,
/// is an error rather than a document no XML parser accepts.
fn write_xml_escaped(out: &mut impl Write, text: &str) -> io::Result<()> {
    let mut plain = 0;
    for (i, c) in text.char_indices() {
        let escaped: &[u8] = match c {
            '&' => b"&amp;",
            '<' => b"&lt;",
            '>' => b"&gt;",
            '"' => b"&quot;",
            '\t' => b"&#9;",
            '\n' => b"&#10;",
            '\r' => b"&#13;",
            '\u{0}'..='\u{1f}' | '\u{fffe}' | '\u{ffff}' => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("U+{:04X} cannot be written in XML 1.0", u32::from(c)),
                ));
            }
            _ => continue,
        };
        out.write_all(&text.as_bytes()[plain..i])?;
        out.write_all(escaped)?;
        plain = i + c.len_utf8();
    }
    out.write_all(&text.as_bytes()[plain..])
}

/// Streams `SELECT` results as SPARQL 1.1 Query Results TSV: a header of
/// `?`-prefixed variable names, then one line per solution with each term
/// in SPARQL syntax and an unbound variable as an empty field (SPARQL 1.1
/// Query Results TSV §4). Lines end in LF, not CRLF as in CSV.
pub struct TsvResultsWriter<W: Write> {
    out: W,
}

impl<W: Write> TsvResultsWriter<W> {
    /// Writes the header line. Variable names cannot hold a tab or a line
    /// break (SPARQL 1.1 Query §19.8, `VARNAME`).
    pub fn start(mut out: W, variables: &[String]) -> io::Result<Self> {
        for (i, variable) in variables.iter().enumerate() {
            if i > 0 {
                out.write_all(b"\t")?;
            }
            out.write_all(b"?")?;
            out.write_all(variable.as_bytes())?;
        }
        out.write_all(b"\n")?;
        Ok(Self { out })
    }

    /// Writes one solution as one line.
    pub fn write(&mut self, solution: &Solution) -> io::Result<()> {
        for (i, value) in solution.values().iter().enumerate() {
            if i > 0 {
                self.out.write_all(b"\t")?;
            }
            match value {
                None => {}
                // IRIs and blank node labels hold no tab or line break:
                // IRIs are validated when built, and labels are hex.
                Some(Term::NamedNode(n)) => write!(self.out, "<{}>", n.as_str())?,
                Some(Term::BlankNode(b)) => write!(self.out, "_:{}", b.as_str())?,
                Some(Term::Literal(l)) => write_tsv_literal(&mut self.out, l)?,
                #[allow(unreachable_patterns)]
                Some(_) => return Err(quoted_triple()),
            }
        }
        self.out.write_all(b"\n")
    }

    /// The underlying writer, e.g. to drain a buffer between solutions.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.out
    }

    /// Returns the writer, unflushed. TSV has no closing part.
    pub fn finish(self) -> io::Result<W> {
        Ok(self.out)
    }
}

/// Writes a literal in SPARQL syntax. Numbers use the abbreviated form
/// that TSV §4.2 recommends, but only when the lexical form reads back as
/// the same datatype: `1.0` as an `xsd:double` would read as a decimal, so
/// it keeps the quoted form.
fn write_tsv_literal(out: &mut impl Write, literal: &Literal) -> io::Result<()> {
    let value = literal.value();
    let datatype = literal.datatype().as_str();
    let abbreviate = literal.language().is_none()
        && match datatype {
            XSD_INTEGER => is_turtle_integer(value),
            XSD_DECIMAL => is_turtle_decimal(value),
            XSD_DOUBLE => is_turtle_double(value),
            _ => false,
        };
    if abbreviate {
        return out.write_all(value.as_bytes());
    }
    // TSV §4.2: the single-quoted form with tab, LF and CR escaped; the
    // quote and the backslash need escaping in any Turtle string.
    out.write_all(b"\"")?;
    let mut plain = 0;
    for (i, c) in value.char_indices() {
        let escaped: &[u8] = match c {
            '"' => b"\\\"",
            '\\' => b"\\\\",
            '\t' => b"\\t",
            '\n' => b"\\n",
            '\r' => b"\\r",
            _ => continue,
        };
        out.write_all(&value.as_bytes()[plain..i])?;
        out.write_all(escaped)?;
        plain = i + 1;
    }
    out.write_all(&value.as_bytes()[plain..])?;
    out.write_all(b"\"")?;
    if let Some(language) = literal.language() {
        write!(out, "@{language}")
    } else if datatype != XSD_STRING {
        write!(out, "^^<{datatype}>")
    } else {
        Ok(())
    }
}

/// Splits off an optional sign and returns the digits that follow, as
/// many as there are.
fn digits(text: &str) -> (&str, &str) {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    text.split_at(end)
}

fn unsigned(text: &str) -> &str {
    text.strip_prefix(['+', '-']).unwrap_or(text)
}

/// Turtle `INTEGER`: `[+-]? [0-9]+`.
fn is_turtle_integer(text: &str) -> bool {
    let (int, rest) = digits(unsigned(text));
    !int.is_empty() && rest.is_empty()
}

/// Turtle `DECIMAL`: `[+-]? [0-9]* '.' [0-9]+`.
fn is_turtle_decimal(text: &str) -> bool {
    let (_, rest) = digits(unsigned(text));
    let Some(fraction) = rest.strip_prefix('.') else {
        return false;
    };
    let (fraction, rest) = digits(fraction);
    !fraction.is_empty() && rest.is_empty()
}

/// Turtle `DOUBLE`: `[+-]? ([0-9]+ '.' [0-9]* EXPONENT | '.'? [0-9]+
/// EXPONENT)`, with `EXPONENT` `[eE] [+-]? [0-9]+`.
fn is_turtle_double(text: &str) -> bool {
    let (int, rest) = digits(unsigned(text));
    let (fraction, rest) = match rest.strip_prefix('.') {
        Some(rest) => digits(rest),
        None => ("", rest),
    };
    let Some(exponent) = rest.strip_prefix(['e', 'E']) else {
        return false;
    };
    let (exponent, rest) = digits(unsigned(exponent));
    (!int.is_empty() || !fraction.is_empty()) && !exponent.is_empty() && rest.is_empty()
}

#[cfg(test)]
mod tests {
    use oxrdf::{BlankNode, Literal, NamedNode};

    use super::*;

    fn write(variables: &[&str], solutions: Vec<Solution>) -> String {
        let variables: Vec<String> = variables.iter().map(|v| (*v).to_owned()).collect();
        let mut writer = JsonResultsWriter::start(Vec::new(), &variables).unwrap();
        for solution in &solutions {
            writer.write(solution).unwrap();
        }
        String::from_utf8(writer.finish().unwrap()).unwrap()
    }

    #[test]
    fn json_results_encode_each_term_kind_and_omit_unbound() {
        let solution = Solution::new(vec![
            Some(NamedNode::new_unchecked("http://ex/a").into()),
            Some(BlankNode::new_unchecked("b0").into()),
            Some(Literal::new_simple_literal("say \"hi\"").into()),
            Some(Literal::new_language_tagged_literal_unchecked("hallo", "de").into()),
            Some(
                Literal::new_typed_literal(
                    "5",
                    NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#integer"),
                )
                .into(),
            ),
            None,
        ]);
        let text = write(&["i", "b", "s", "l", "t", "u"], vec![solution]);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "head": { "vars": ["i", "b", "s", "l", "t", "u"] },
                "results": { "bindings": [{
                    "i": { "type": "uri", "value": "http://ex/a" },
                    "b": { "type": "bnode", "value": "b0" },
                    "s": { "type": "literal", "value": "say \"hi\"" },
                    "l": { "type": "literal", "value": "hallo", "xml:lang": "de" },
                    "t": {
                        "type": "literal",
                        "value": "5",
                        "datatype": "http://www.w3.org/2001/XMLSchema#integer"
                    },
                }]},
            })
        );
    }

    fn write_csv(variables: &[&str], solutions: Vec<Solution>) -> String {
        let variables: Vec<String> = variables.iter().map(|v| (*v).to_owned()).collect();
        let mut writer = CsvResultsWriter::start(Vec::new(), &variables).unwrap();
        for solution in &solutions {
            writer.write(solution).unwrap();
        }
        String::from_utf8(writer.finish().unwrap()).unwrap()
    }

    #[test]
    fn csv_results_write_lexical_forms_and_unbound_as_empty() {
        let iri = || Some(NamedNode::new_unchecked("http://example/x").into());
        let solutions = vec![
            Solution::new(vec![
                iri(),
                Some(Literal::new_simple_literal("String").into()),
            ]),
            Solution::new(vec![
                iri(),
                Some(Literal::new_simple_literal("String-with-dquote\"").into()),
            ]),
            Solution::new(vec![
                Some(BlankNode::new_unchecked("b0").into()),
                Some(Literal::new_simple_literal("Blank node").into()),
            ]),
            Solution::new(vec![None, Some(Literal::new_simple_literal("a,b").into())]),
            Solution::new(vec![None, None]),
            Solution::new(vec![
                Some(BlankNode::new_unchecked("b1").into()),
                Some(
                    Literal::new_language_tagged_literal_unchecked("String-with-lang", "en").into(),
                ),
            ]),
            Solution::new(vec![
                iri(),
                Some(
                    Literal::new_typed_literal(
                        "123",
                        NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#integer"),
                    )
                    .into(),
                ),
            ]),
            Solution::new(vec![
                iri(),
                Some(Literal::new_simple_literal("two\nlines").into()),
            ]),
        ];
        // The example of SPARQL 1.1 Query Results CSV §2, plus a comma and
        // a line break.
        assert_eq!(
            write_csv(&["x", "literal"], solutions),
            "x,literal\r\n\
             http://example/x,String\r\n\
             http://example/x,\"String-with-dquote\"\"\"\r\n\
             _:b0,Blank node\r\n\
             ,\"a,b\"\r\n\
             ,\r\n\
             _:b1,String-with-lang\r\n\
             http://example/x,123\r\n\
             http://example/x,\"two\nlines\"\r\n"
        );
    }

    #[test]
    fn csv_record_of_one_empty_field_is_quoted() {
        assert_eq!(
            write_csv(&["x"], vec![Solution::new(vec![None])]),
            "x\r\n\"\"\r\n"
        );
    }

    #[test]
    fn json_results_without_solutions_is_valid() {
        let text = write(&["x"], Vec::new());
        assert_eq!(
            text,
            "{\"head\":{\"vars\":[\"x\"]},\"results\":{\"bindings\":[\n]}}\n"
        );
    }

    #[test]
    fn json_results_put_one_solution_per_line() {
        let a = Solution::new(vec![Some(Literal::new_simple_literal("a").into())]);
        let b = Solution::new(vec![None]);
        assert_eq!(
            write(&["x"], vec![a, b]),
            "{\"head\":{\"vars\":[\"x\"]},\"results\":{\"bindings\":[\n\
             {\"x\":{\"type\":\"literal\",\"value\":\"a\"}},\n{}\n]}}\n"
        );
    }
    fn variables(names: &[&str]) -> Vec<String> {
        names.iter().map(|v| (*v).to_owned()).collect()
    }

    fn write_in(format: ResultsFormat, names: &[&str], solutions: &[Solution]) -> String {
        let mut writer = ResultsWriter::start(format, Vec::new(), &variables(names)).unwrap();
        for solution in solutions {
            writer.write(solution).unwrap();
        }
        String::from_utf8(writer.finish().unwrap()).unwrap()
    }

    fn typed(value: &str, datatype: &str) -> Option<Term> {
        Some(Literal::new_typed_literal(value, NamedNode::new_unchecked(datatype)).into())
    }

    fn simple(value: &str) -> Option<Term> {
        Some(Literal::new_simple_literal(value).into())
    }

    #[test]
    fn each_format_names_its_media_type() {
        let types: Vec<(&str, &str)> = ResultsFormat::ALL
            .iter()
            .map(|f| (f.media_type(), f.content_type()))
            .collect();
        assert_eq!(
            types,
            [
                (
                    "application/sparql-results+json",
                    "application/sparql-results+json"
                ),
                (
                    "application/sparql-results+xml",
                    "application/sparql-results+xml"
                ),
                ("text/csv", "text/csv; charset=utf-8"),
                (
                    "text/tab-separated-values",
                    "text/tab-separated-values; charset=utf-8"
                ),
            ]
        );
    }

    #[test]
    fn results_writer_matches_each_format_writer() {
        let solutions = [Solution::new(vec![simple("a")]), Solution::new(vec![None])];
        assert_eq!(
            write_in(ResultsFormat::Json, &["x"], &solutions),
            write(&["x"], solutions.to_vec())
        );
        assert_eq!(
            write_in(ResultsFormat::Csv, &["x"], &solutions),
            write_csv(&["x"], solutions.to_vec())
        );
    }

    #[test]
    fn every_format_writes_an_empty_result_with_its_head() {
        let empty = |format| write_in(format, &["x", "y"], &[]);
        assert_eq!(
            empty(ResultsFormat::Json),
            "{\"head\":{\"vars\":[\"x\",\"y\"]},\"results\":{\"bindings\":[\n]}}\n"
        );
        assert_eq!(
            empty(ResultsFormat::Xml),
            "<?xml version=\"1.0\"?>\n\
             <sparql xmlns=\"http://www.w3.org/2005/sparql-results#\"><head>\
             <variable name=\"x\"/><variable name=\"y\"/></head><results>\n\
             </results></sparql>\n"
        );
        assert_eq!(empty(ResultsFormat::Csv), "x,y\r\n");
        assert_eq!(empty(ResultsFormat::Tsv), "?x\t?y\n");
    }

    #[test]
    fn xml_results_encode_each_term_kind_and_omit_unbound() {
        let solution = Solution::new(vec![
            Some(NamedNode::new_unchecked("http://ex/a?b=1&c=2").into()),
            Some(BlankNode::new_unchecked("b0").into()),
            simple("a < b & c > \"d\""),
            Some(Literal::new_language_tagged_literal_unchecked("hallo", "de").into()),
            typed("5", XSD_INTEGER),
            None,
            typed("x", XSD_STRING),
        ]);
        assert_eq!(
            write_in(
                ResultsFormat::Xml,
                &["i", "b", "s", "l", "t", "u", "x"],
                &[solution, Solution::new(vec![None; 7])]
            ),
            "<?xml version=\"1.0\"?>\n\
             <sparql xmlns=\"http://www.w3.org/2005/sparql-results#\"><head>\
             <variable name=\"i\"/><variable name=\"b\"/><variable name=\"s\"/>\
             <variable name=\"l\"/><variable name=\"t\"/><variable name=\"u\"/>\
             <variable name=\"x\"/></head><results>\n\
             <result>\
             <binding name=\"i\"><uri>http://ex/a?b=1&amp;c=2</uri></binding>\
             <binding name=\"b\"><bnode>b0</bnode></binding>\
             <binding name=\"s\"><literal>a &lt; b &amp; c &gt; &quot;d&quot;</literal></binding>\
             <binding name=\"l\"><literal xml:lang=\"de\">hallo</literal></binding>\
             <binding name=\"t\"><literal datatype=\"http://www.w3.org/2001/XMLSchema#integer\">5</literal></binding>\
             <binding name=\"x\"><literal>x</literal></binding>\
             </result>\n\
             <result></result>\n\
             </results></sparql>\n"
        );
    }

    #[test]
    fn xml_results_keep_whitespace_and_refuse_characters_xml_cannot_hold() {
        assert_eq!(
            write_in(
                ResultsFormat::Xml,
                &["s"],
                &[Solution::new(vec![simple("a\tb\r\nc ü 😀")])]
            )
            .lines()
            .nth(2),
            Some(
                "<result><binding name=\"s\"><literal>a&#9;b&#13;&#10;c ü 😀</literal></binding></result>"
            )
        );
        for text in ["bell\u{7}", "\u{0}", "\u{ffff}"] {
            let mut writer = XmlResultsWriter::start(Vec::new(), &variables(&["s"])).unwrap();
            let error = writer
                .write(&Solution::new(vec![simple(text)]))
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{text:?}");
        }
    }

    #[test]
    fn tsv_results_write_the_specification_example() {
        let iri = || Some(NamedNode::new_unchecked("http://example/x").into());
        let blank = |label| Some(BlankNode::new_unchecked(label).into());
        let solutions = [
            Solution::new(vec![iri(), simple("String")]),
            Solution::new(vec![iri(), simple("String-with-dquote\"")]),
            Solution::new(vec![blank("blank0"), simple("Blank node")]),
            Solution::new(vec![None, simple("Missing 'x'")]),
            Solution::new(vec![None, None]),
            Solution::new(vec![iri(), None]),
            Solution::new(vec![
                blank("blank1"),
                Some(
                    Literal::new_language_tagged_literal_unchecked("String-with-lang", "en").into(),
                ),
            ]),
            Solution::new(vec![blank("blank1"), typed("123", XSD_INTEGER)]),
        ];
        // SPARQL 1.1 Query Results TSV §4.3.
        assert_eq!(
            write_in(ResultsFormat::Tsv, &["x", "literal"], &solutions),
            "?x\t?literal\n\
             <http://example/x>\t\"String\"\n\
             <http://example/x>\t\"String-with-dquote\\\"\"\n\
             _:blank0\t\"Blank node\"\n\
             \t\"Missing 'x'\"\n\
             \t\n\
             <http://example/x>\t\n\
             _:blank1\t\"String-with-lang\"@en\n\
             _:blank1\t123\n"
        );
    }

    #[test]
    fn tsv_results_escape_literals_and_keep_datatypes() {
        let solutions = [
            simple("tab\there\nnew line\r\\ \"q\""),
            typed("2026-10-28", "http://www.w3.org/2001/XMLSchema#date"),
            typed("plain", XSD_STRING),
            typed("-1.50", XSD_DECIMAL),
            typed(".5", XSD_DECIMAL),
            typed("1.0E0", XSD_DOUBLE),
            typed("-.5e-3", XSD_DOUBLE),
            typed("+7", XSD_INTEGER),
            // Abbreviated, these would read back as another datatype or
            // not at all.
            typed("1.0", XSD_DOUBLE),
            typed("1", XSD_DECIMAL),
            typed("INF", XSD_DOUBLE),
            typed("NaN", XSD_DOUBLE),
            typed("1.", XSD_DECIMAL),
            typed("x1", XSD_INTEGER),
            typed("1e", XSD_DOUBLE),
            typed("", XSD_INTEGER),
        ]
        .map(|term| Solution::new(vec![term]));
        let text = write_in(ResultsFormat::Tsv, &["v"], &solutions);
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(
            lines,
            [
                "?v",
                "\"tab\\there\\nnew line\\r\\\\ \\\"q\\\"\"",
                "\"2026-10-28\"^^<http://www.w3.org/2001/XMLSchema#date>",
                "\"plain\"",
                "-1.50",
                ".5",
                "1.0E0",
                "-.5e-3",
                "+7",
                "\"1.0\"^^<http://www.w3.org/2001/XMLSchema#double>",
                "\"1\"^^<http://www.w3.org/2001/XMLSchema#decimal>",
                "\"INF\"^^<http://www.w3.org/2001/XMLSchema#double>",
                "\"NaN\"^^<http://www.w3.org/2001/XMLSchema#double>",
                "\"1.\"^^<http://www.w3.org/2001/XMLSchema#decimal>",
                "\"x1\"^^<http://www.w3.org/2001/XMLSchema#integer>",
                "\"1e\"^^<http://www.w3.org/2001/XMLSchema#double>",
                "\"\"^^<http://www.w3.org/2001/XMLSchema#integer>",
                "",
            ]
        );
    }

    #[test]
    fn tsv_results_write_an_unbound_single_variable_as_an_empty_line() {
        assert_eq!(
            write_in(ResultsFormat::Tsv, &["x"], &[Solution::new(vec![None])]),
            "?x\n\n"
        );
    }
}
