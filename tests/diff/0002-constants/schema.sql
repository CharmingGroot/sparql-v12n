-- Regression cases for query constants (#16): constants compared with
-- integer, date, uuid and numeric columns, and one predicate produced by
-- two triples maps with different column types.
CREATE TABLE readings (
  id          integer       PRIMARY KEY,
  value       integer       NOT NULL,
  measured_on date          NOT NULL,
  device      uuid          NOT NULL,
  price       numeric(10,2) NOT NULL
);

CREATE TABLE labels (
  id    integer PRIMARY KEY,
  value text    NOT NULL
);
