INSERT INTO people VALUES
  (1, 'Ann', 'ann@example.org', NULL),
  (2, 'Bo',  NULL,              1),
  (3, 'Cy',  'cy@example.org',  1),
  (4, 'Di',  NULL,              99);

INSERT INTO contractors VALUES
  ('c1', 'Eve', 'Acme'),
  ('c2', 'Fay', NULL);

INSERT INTO vendors VALUES
  (1, 'Gus');

-- Order 13 has no person, order 14 one that does not exist.
INSERT INTO orders VALUES
  (10, 1,    100, 'rush'),
  (11, 1,    250, NULL),
  (12, 3,    75,  'gift'),
  (13, NULL, 40,  NULL),
  (14, 99,   60,  'lost');

-- The same phone twice for person 1: one triple.
INSERT INTO phones VALUES
  (1, '555-0100'),
  (1, '555-0100'),
  (1, '555-0101'),
  (3, '555-0300');
