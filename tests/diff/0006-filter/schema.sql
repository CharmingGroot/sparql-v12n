-- FILTER (#50): values of each type the operator mapping compares, NULLs
-- for unbound variables, and a part to put an OPTIONAL over.
CREATE TABLE items (
  id        integer PRIMARY KEY,
  name      text    NOT NULL,
  qty       integer,
  stock     bigint,
  price     numeric(10, 2),
  weight    double precision,
  active    boolean,
  created   timestamp,
  updated   timestamptz,
  released  date,
  label     text,
  parent_id integer
);

CREATE TABLE tags (
  item_id integer NOT NULL,
  tag     text    NOT NULL
);
