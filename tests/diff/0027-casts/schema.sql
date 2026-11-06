-- XPath constructor functions (#107): strings with valid and invalid
-- lexical forms, surrounding whitespace and signs, integers, decimals,
-- doubles, booleans and timestamps. No NaN: Ontop compares doubles in
-- PostgreSQL, which orders NaN above every number (#99).
CREATE TABLE vals (
  id  integer           PRIMARY KEY,
  s   text              NOT NULL,
  i   integer           NOT NULL,
  d   numeric(6,2)      NOT NULL,
  f   double precision  NOT NULL,
  b   boolean           NOT NULL,
  t   timestamp         NOT NULL
);
