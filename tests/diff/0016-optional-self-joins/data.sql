-- No asset has an owner. Asset 4 has the same hostname and site, asset 6
-- the same criticality and site, asset 5 neither criticality nor site.
INSERT INTO assets VALUES
  (1, 'web1', 'High', 'x',  NULL),
  (2, 'db1',  'High', NULL, NULL),
  (3, 'mail', NULL,   'y',  NULL),
  (4, 'x',    'Low',  'x',  NULL),
  (5, 'dns',  NULL,   NULL, NULL),
  (6, 'lab',  'y',    'y',  NULL);

INSERT INTO alerts VALUES
  (10, 1, 'Brute force'),
  (11, 1, NULL),
  (12, 3, 'Phishing');
