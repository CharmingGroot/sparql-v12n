# W3C RDB2RDF test cases (subset, unmodified)

Files from **R2RML and Direct Mapping Test Cases** (W3C Editor's Draft
24 July 2012), <https://www.w3.org/2001/sw/rdb2rdf/test-cases/>, published
as the W3C Note <https://www.w3.org/TR/rdb2rdf-test-cases/>.

Copyright © 2012 W3C® (MIT, ERCIM, Keio), All Rights Reserved.
Distributed under the W3C Document License,
<https://www.w3.org/copyright/document-license/> (linked from the
test-cases page as <https://www.w3.org/Consortium/Legal/copyright-documents>).

- Source: `https://dvcs.w3.org/hg/rdb2rdf-tests/raw-file/default/<directory>/<file>`,
  retrieved 2026-10-23.
- The files are copied byte for byte and must not be edited. `SHA256SUMS`
  lists their SHA-256 digests; check with `shasum -a 256 -c SHA256SUMS`.
- Only the databases and R2RML tests that the differential tests use are
  included: each directory has the test database's `create.sql`, its
  `manifest.ttl`, and the mapping (`r2rml*.ttl`) and expected output
  (`mapped*.nq`) of each included test.

The queries and expectations that run these tests are this project's own
files, in `tests/diff/w3c/<test id>/`. `tests/diff/w3c/README.md` lists the
included tests and the skipped ones with the reason.
