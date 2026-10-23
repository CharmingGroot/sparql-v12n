// SPDX-License-Identifier: Apache-2.0
//! SPARQL 1.1 Query Results JSON Format, written one solution at a time.

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

    /// Closes the document and returns the writer, unflushed.
    pub fn finish(mut self) -> io::Result<W> {
        self.out.write_all(b"\n]}}\n")?;
        Ok(self.out)
    }
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
