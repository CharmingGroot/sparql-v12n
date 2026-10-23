-- OPTIONAL as a left join (#49): nullable columns, references that may
-- not resolve, classes from several triples maps, and a table without a
-- key that repeats rows.
CREATE TABLE people (
  id         integer PRIMARY KEY,
  name       text    NOT NULL,
  email      text,
  manager_id integer
);

CREATE TABLE contractors (
  code   text PRIMARY KEY,
  name   text NOT NULL,
  agency text
);

CREATE TABLE vendors (
  id   integer PRIMARY KEY,
  name text    NOT NULL
);

CREATE TABLE orders (
  id        integer PRIMARY KEY,
  person_id integer,
  amount    integer NOT NULL,
  note      text
);

CREATE INDEX orders_person_id ON orders (person_id);

CREATE TABLE phones (
  person_id integer NOT NULL,
  phone     text    NOT NULL
);
