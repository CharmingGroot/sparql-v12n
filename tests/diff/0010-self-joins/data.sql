-- Asset 4 has the same hostname and site, asset 2 no site, asset 3 no
-- criticality.
INSERT INTO assets VALUES
  (1, 'web1', 'High', 'x'),
  (2, 'db1',  'High', NULL),
  (3, 'mail', NULL,   'y'),
  (4, 'x',    'Low',  'x');

INSERT INTO alerts VALUES
  (10, 1, 'Brute force', 5),
  (11, 1, 'Port scan',   2),
  (12, 2, 'Malware',     4),
  (13, 3, 'Phishing',    3);

INSERT INTO links VALUES
  (1, 2, 'a-b'),
  (1, 3, NULL),
  (2, 3, 'b-c');

INSERT INTO badges VALUES
  ('k1', 'one',  'kim'),
  (NULL, 'none', 'lee'),
  (NULL, 'void', 'max'),
  ('k2', 'two',  NULL);

INSERT INTO teams VALUES
  (1, 'a', 'x1', 'y1'),
  (1, 'B', 'x2', 'y2'),
  (2, 'c', 'x3', NULL);

INSERT INTO people VALUES
  (1, 'kim@example.org', 'Kim', 'red'),
  (2, 'lee@example.org', 'Lee', NULL);
