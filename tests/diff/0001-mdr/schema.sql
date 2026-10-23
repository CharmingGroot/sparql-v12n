-- Security monitoring (MDR) example: assets, detection findings on them,
-- and network links between assets. Column names and values follow OCSF
-- (Detection Finding: finding_info.title, severity_id; resource_details:
-- hostname, criticality).
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

CREATE INDEX alerts_asset_id ON alerts (asset_id);

CREATE TABLE links (
  src_id integer NOT NULL REFERENCES assets (id),
  dst_id integer NOT NULL REFERENCES assets (id),
  PRIMARY KEY (src_id, dst_id)
);
