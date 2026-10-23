-- Asset 10 is not classified yet: its criticality is NULL.
INSERT INTO assets (id, hostname, criticality) VALUES
  (7, 'web-01', 'High'),
  (8, 'dev-02', 'Low'),
  (9, 'db-01', 'High'),
  (10, 'lab-03', NULL);

INSERT INTO alerts (id, asset_id, title, severity_id) VALUES
  (1001, 7, 'ssh-bruteforce', 4),
  (1002, 8, 'port-scan', 2),
  (1003, 9, 'sql-injection', 5),
  (1004, 7, 'malware-beacon', 3);

-- dev-02 -> web-01 -> db-01
INSERT INTO links (src_id, dst_id) VALUES
  (8, 7),
  (7, 9);
