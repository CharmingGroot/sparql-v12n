-- Person 1 is in `people` twice; person 2 has no city.
INSERT INTO people (id, name, city) VALUES
  (1, 'Ann', 'Oslo'),
  (1, 'Ann', 'Oslo'),
  (2, 'Bob', NULL);

INSERT INTO pets (id, owner, kind) VALUES
  (10, 1, 'cat');
