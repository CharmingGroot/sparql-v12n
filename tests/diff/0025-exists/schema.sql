-- EXISTS and NOT EXISTS (#115): patterns tested with the variables of the
-- current solution substituted, compared as RDF terms.
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
