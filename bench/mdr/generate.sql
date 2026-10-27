-- Deterministic MDR data: no random(), so every run and every machine
-- loads the same rows. Run with psql -v alerts=N (number of alerts).
--   assets       N / 10; criticality High 20%, Medium 30%, Low 40%, NULL 10%
--   alerts       N; severity 5 for 2%, 4 for 8%, 3 for 15%, 2 for 25%, 1 for 50%
--   links        about 2 per asset
--   observables  2 per alert, every fifth alert records its first one twice
\set ON_ERROR_STOP on

INSERT INTO assets (id, hostname, criticality)
SELECT i,
       (ARRAY['web', 'db', 'dev', 'lab', 'mail', 'vpn'])[1 + i % 6] || '-' || lpad(i::text, 7, '0'),
       CASE
         WHEN i % 10 = 0 THEN NULL
         WHEN i % 10 IN (1, 2) THEN 'High'
         WHEN i % 10 IN (3, 4, 5) THEN 'Medium'
         ELSE 'Low'
       END
FROM generate_series(1, :alerts / 10) AS i;

INSERT INTO alerts (id, asset_id, title, severity_id)
SELECT i,
       1 + (i::bigint * 7919) % (:alerts / 10),
       (ARRAY['ssh-bruteforce', 'port-scan', 'sql-injection', 'malware-beacon',
              'dns-tunnel', 'privilege-escalation', 'phishing-click', 'data-exfiltration'])[1 + (i * 5) % 8],
       CASE
         WHEN i % 50 = 0 THEN 5
         WHEN i % 10 = 0 THEN 4
         WHEN i % 4 = 0 THEN 3
         WHEN i % 2 = 0 THEN 2
         ELSE 1
       END
FROM generate_series(1, :alerts) AS i;

INSERT INTO links (src_id, dst_id)
SELECT i, 1 + (i::bigint * k) % (:alerts / 10)
FROM generate_series(1, :alerts / 10) AS i, (VALUES (31), (97)) AS m(k)
ON CONFLICT DO NOTHING;

INSERT INTO observables (alert_id, value)
SELECT i, 'ioc-' || (i::bigint * k + k) % (:alerts / 20)
FROM generate_series(1, :alerts) AS i, (VALUES (7), (11)) AS m(k);

INSERT INTO observables (alert_id, value)
SELECT i, 'ioc-' || (i::bigint * 7 + 7) % (:alerts / 20)
FROM generate_series(5, :alerts, 5) AS i;

CREATE INDEX alerts_asset_id ON alerts (asset_id);
CREATE INDEX alerts_severity_id ON alerts (severity_id);
CREATE INDEX assets_criticality ON assets (criticality);
CREATE INDEX links_dst_id ON links (dst_id);
CREATE INDEX observables_alert_id ON observables (alert_id);

VACUUM ANALYZE;
