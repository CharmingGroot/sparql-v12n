-- Self-join elimination (#28): tables scanned more than once by one
-- query, with keys that pin the scans to one row and keys that do not.
CREATE TABLE assets (
  id          integer PRIMARY KEY,
  hostname    text    NOT NULL,
  criticality text,
  site        text
);

CREATE TABLE alerts (
  id          integer PRIMARY KEY,
  asset_id    integer NOT NULL REFERENCES assets (id),
  title       text    NOT NULL,
  severity_id integer NOT NULL
);

CREATE TABLE links (
  src_id integer NOT NULL,
  dst_id integer NOT NULL,
  note   text,
  PRIMARY KEY (src_id, dst_id)
);

-- A unique key on a nullable column: two rows may both be NULL.
CREATE TABLE badges (
  code   text UNIQUE,
  label  text,
  holder text
);

-- Unique over an expression: not a key of the columns themselves.
CREATE TABLE teams (
  grp  integer NOT NULL,
  name text    NOT NULL,
  x    text,
  y    text
);
CREATE UNIQUE INDEX teams_grp_name ON teams (grp, lower(name));

-- A unique key that is not the primary key.
CREATE TABLE people (
  id    integer PRIMARY KEY,
  email text    NOT NULL UNIQUE,
  name  text,
  team  text
);
