-- Sources of duplicate triples (#32): a table without a key that repeats a
-- row, a second table mapped to the same triples, and a keyed table.
CREATE TABLE people (
  id   integer,
  name text,
  city text
);

CREATE TABLE staff (
  id   integer PRIMARY KEY,
  name text    NOT NULL
);

CREATE TABLE accounts (
  id    integer PRIMARY KEY,
  owner integer NOT NULL,
  kind  text    NOT NULL
);
