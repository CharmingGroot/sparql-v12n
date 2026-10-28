-- Arithmetic in FILTER (#99): integers, decimals and an int8 value at the
-- edge of its range. No divisor is zero: Ontop sends the division to
-- PostgreSQL as it is, which fails or not depending on the order in which
-- the plan evaluates the rows. Division by zero is covered by the
-- PostgreSQL tests of v12n instead.
CREATE TABLE readings (
  id    integer       PRIMARY KEY,
  name  text          NOT NULL,
  a     integer       NOT NULL,
  b     integer       NOT NULL,
  d     numeric(6,2)  NOT NULL,
  big   bigint        NOT NULL
);
