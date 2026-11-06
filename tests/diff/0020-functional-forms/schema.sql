-- IF, COALESCE, IN and NOT IN (#101): integers, strings, a divisor that is
-- zero in one row and a column that is NULL in some rows.
CREATE TABLE items (
  id    integer  PRIMARY KEY,
  name  text     NOT NULL,
  a     integer  NOT NULL,
  b     integer  NOT NULL,
  opt   integer
);
