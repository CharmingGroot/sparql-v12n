-- Self-joins across OPTIONAL (#151): optional parts that can only match
-- the row of a required scan by its primary key.
CREATE TABLE assets (
  id          integer PRIMARY KEY,
  hostname    text    NOT NULL,
  criticality text,
  site        text,
  owner       text
);

CREATE TABLE alerts (
  id       integer PRIMARY KEY,
  asset_id integer NOT NULL REFERENCES assets (id),
  title    text
);
