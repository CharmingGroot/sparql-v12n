INSERT INTO people (id, name, city, code, score, bonus) VALUES
  (1, 'Kim',  'Seoul', '3',  3,    3),
  (2, 'Lee',  NULL,    '5',  5,    1),
  (3, 'Park', 'Busan', NULL, 1,    NULL),
  (4, 'Choi', 'Daegu', '1',  NULL, 2);

INSERT INTO staff (id, name) VALUES
  (3, 'P. Park'),
  (5, 'Jung');

INSERT INTO orders (id, person_id, amount, ship_city) VALUES
  (10, 1, 3, 'Seoul'),
  (11, 1, 7, NULL),
  (12, 2, 3, 'Busan'),
  (13, 3, 1, 'Incheon');
