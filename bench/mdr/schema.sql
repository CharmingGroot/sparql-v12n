-- The MDR example of tests/diff/0001-mdr at benchmark scale, plus
-- observables: a log-like table without a key, where the same observable
-- can be recorded twice for one finding, and two customer tables whose
-- triples maps share predicates and a subject template. Indexes other than
-- the keys are created by generate.sql after loading, as an operator would
-- add them.
CREATE TABLE assets (
  id          integer PRIMARY KEY,
  hostname    text    NOT NULL,
  criticality text
);

CREATE TABLE alerts (
  id          integer PRIMARY KEY,
  asset_id    integer NOT NULL REFERENCES assets (id),
  title       text    NOT NULL,
  severity_id integer NOT NULL
);

CREATE TABLE links (
  src_id integer NOT NULL REFERENCES assets (id),
  dst_id integer NOT NULL REFERENCES assets (id),
  PRIMARY KEY (src_id, dst_id)
);

CREATE TABLE observables (
  alert_id integer NOT NULL REFERENCES alerts (id),
  value    text    NOT NULL
);

-- Two sources of the same customers: the triples maps over both define the
-- same predicates with the same subject template, so every triple pattern
-- of a basic graph pattern over them has two candidates (issue #222).
CREATE TABLE crm_customers (
  id      integer PRIMARY KEY,
  name    text    NOT NULL,
  email   text    NOT NULL,
  country text    NOT NULL,
  segment text    NOT NULL
);

CREATE TABLE billing_customers (
  id      integer PRIMARY KEY,
  name    text    NOT NULL,
  email   text    NOT NULL,
  country text    NOT NULL,
  segment text    NOT NULL
);
