INSERT INTO people (id, name, city, born) VALUES
  (1, 'Kim',  'Seoul', DATE '1990-01-02'),
  (2, 'Lee',  NULL,    DATE '1985-05-06'),
  (3, 'Park', 'Busan', DATE '2000-12-31');

INSERT INTO orders (id, person_id, amount, price) VALUES
  (10, 1, 3, 12.50),
  (11, 1, 5,  7.25),
  (12, 2, 3,  7.25),
  (13, 3, 1, 12.50);
