-- The MDR example of tests/diff/0001-mdr at benchmark scale, plus
-- observables: a log-like table without a key, where the same observable
-- can be recorded twice for one finding. Indexes other than the keys are
-- created by generate.sql after loading, as an operator would add them.
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
