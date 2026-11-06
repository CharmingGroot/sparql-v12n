-- MINUS (#114): anti-joins on shared variables compared as RDF terms,
-- with variables that OPTIONAL leaves unbound on either side.
CREATE TABLE people (
  id     integer  PRIMARY KEY,
  name   text     NOT NULL,
  city   text,
  code   text,
  score  smallint,
  bonus  integer
);

CREATE TABLE staff (
  id    integer PRIMARY KEY,
  name  text    NOT NULL
);

CREATE TABLE orders (
  id         integer PRIMARY KEY,
  person_id  integer NOT NULL REFERENCES people (id),
  amount     integer NOT NULL,
  ship_city  text
);
