// SPDX-License-Identifier: Apache-2.0
//! SPARQL 1.1 Query Results JSON and CSV Formats, written one solution at a
//! time.

use std::io::{self, Write};

use oxrdf::Term;

use crate::solution::Solution;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// Media type of the SPARQL 1.1 Query Results JSON Format.
pub const JSON_RESULTS_MEDIA_TYPE: &str = "application/sparql-results+json";

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

/// Media type of the SPARQL 1.1 Query Results CSV Format.
pub const CSV_RESULTS_MEDIA_TYPE: &str = "text/csv; charset=utf-8";

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
                Some(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "quoted triples cannot be written as SPARQL 1.1 results",
                    ));
                }
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
        // Quoted triples exist only with oxrdf's `rdf-12` feature.
        #[allow(unreachable_patterns)]
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "quoted triples cannot be written as SPARQL 1.1 results",
            ));
        }
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
}
