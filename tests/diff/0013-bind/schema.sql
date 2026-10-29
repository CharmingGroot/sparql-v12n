-- BIND and SELECT expressions (#100): integers, decimals, a string, a
-- column that is NULL in some rows and an int8 value at the edge of its
-- range. No divisor is zero (see 0012-arithmetic).
CREATE TABLE items (
  id    integer       PRIMARY KEY,
  name  text          NOT NULL,
  a     integer       NOT NULL,
  b     integer       NOT NULL,
  d     numeric(6,2)  NOT NULL,
  big   bigint        NOT NULL,
  opt   integer
);
