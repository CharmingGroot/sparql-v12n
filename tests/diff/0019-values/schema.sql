-- VALUES (#113): inline data joined with patterns over integer, text,
-- numeric and date columns and IRI templates, with UNDEF.
CREATE TABLE people (
  id    integer PRIMARY KEY,
  name  text    NOT NULL,
  city  text,
  born  date    NOT NULL
);

CREATE TABLE orders (
  id         integer      PRIMARY KEY,
  person_id  integer      NOT NULL REFERENCES people (id),
  amount     integer      NOT NULL,
  price      numeric(8,2) NOT NULL
);
