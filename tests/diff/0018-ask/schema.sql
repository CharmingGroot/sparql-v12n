-- ASK (#117): a table without a key that repeats a row, a column with
-- NULLs, and a keyed table that refers to it.
CREATE TABLE people (
  id   integer,
  name text,
  city text
);

CREATE TABLE pets (
  id    integer PRIMARY KEY,
  owner integer NOT NULL,
  kind  text    NOT NULL
);
