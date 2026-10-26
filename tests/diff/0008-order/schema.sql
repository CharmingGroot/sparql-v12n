-- ORDER BY, LIMIT and OFFSET (#52): values of each kind SPARQL orders,
-- ties, NULLs for unbound variables, and strings beyond ASCII.
CREATE TABLE products (
  id        integer PRIMARY KEY,
  name      text    NOT NULL,
  category  text,
  price     integer,
  weight    numeric,
  rating    double precision,
  in_stock  boolean,
  added     timestamp,
  label     text
);

CREATE TABLE reviews (
  id         integer PRIMARY KEY,
  product_id integer NOT NULL REFERENCES products (id),
  stars      integer NOT NULL,
  note       text
);
