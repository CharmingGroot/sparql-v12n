-- Arithmetic on xsd:double values (#99): float8 columns with -0 and the
-- infinities, NaN in its own column, integers and decimals promoted to
-- doubles. No operation overflows, rounds to zero or divides by zero:
-- PostgreSQL raises an error for those, which aborts Ontop's
-- transaction. The PostgreSQL tests of v12n cover them instead.
CREATE TABLE measures (
  id    integer           PRIMARY KEY,
  name  text              NOT NULL,
  f     double precision  NOT NULL,
  g     double precision  NOT NULL,
  n     double precision  NOT NULL,
  i     integer           NOT NULL,
  d     numeric(6,2)      NOT NULL
);
