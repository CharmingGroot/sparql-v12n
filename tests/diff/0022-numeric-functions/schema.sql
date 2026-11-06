-- ABS, ROUND, CEIL and FLOOR (#104): integers, decimals and doubles with
-- halves of both signs, where fn:round rounds towards positive infinity,
-- and a string, which is a type error. No NaN: Ontop compares doubles in
-- PostgreSQL, which orders NaN above every number (#99).
CREATE TABLE nums (
  id    integer           PRIMARY KEY,
  name  text              NOT NULL,
  i     integer           NOT NULL,
  d     numeric(6,2)      NOT NULL,
  f     double precision  NOT NULL
);
