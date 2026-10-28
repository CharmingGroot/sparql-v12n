-- Arithmetic in FILTER (#99): integers, decimals, a zero divisor and an
-- int8 value at the edge of its range.
CREATE TABLE readings (
  id    integer       PRIMARY KEY,
  name  text          NOT NULL,
  a     integer       NOT NULL,
  b     integer       NOT NULL,
  d     numeric(6,2)  NOT NULL,
  big   bigint        NOT NULL
);
