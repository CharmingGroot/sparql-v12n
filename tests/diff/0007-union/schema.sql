-- UNION (#51) and variables whose term maps differ in shape (#29): IRIs
-- from templates with different prefixes and from a column that repeats
-- some of them, constants, IRIs against literals, language tags and
-- datatypes.
CREATE TABLE customers (
  id   integer PRIMARY KEY,
  name text    NOT NULL,
  city text
);

CREATE TABLE wholesalers (
  code     text PRIMARY KEY,
  name     text NOT NULL,
  homepage text
);

-- No key, and one row twice: subjects from an IRI column, some of which
-- are customers' IRIs.
CREATE TABLE partners (
  iri  text NOT NULL,
  name text NOT NULL
);

CREATE TABLE orders (
  id          integer PRIMARY KEY,
  customer_id integer,
  amount      integer NOT NULL,
  status      text    NOT NULL
);

CREATE TABLE notes (
  id       integer PRIMARY KEY,
  order_id integer NOT NULL,
  body     text    NOT NULL,
  lang     text    NOT NULL
);
