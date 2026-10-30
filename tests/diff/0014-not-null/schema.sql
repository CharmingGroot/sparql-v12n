-- IS NOT NULL left out on NOT NULL columns (#111): required and optional
-- columns with and without the constraint, a view whose columns can be
-- NULL although the table's cannot, and parts that do not match.
CREATE TABLE items (
  id    integer PRIMARY KEY,
  label text    NOT NULL,
  note  text,
  owner integer
);

CREATE TABLE owners (
  id   integer PRIMARY KEY,
  name text    NOT NULL,
  city text
);

CREATE VIEW labelled AS
  SELECT id, CASE WHEN id % 2 = 0 THEN label END AS label FROM items;
