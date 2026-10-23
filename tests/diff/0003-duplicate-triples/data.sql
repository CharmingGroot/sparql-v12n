-- Person 1 is in `people` twice and in `staff` once.
INSERT INTO people (id, name, city) VALUES
  (1, 'Ann', 'Oslo'),
  (1, 'Ann', 'Oslo'),
  (2, 'Bob', 'Rome'),
  (3, 'Cy', 'Oslo');

INSERT INTO staff (id, name) VALUES
  (1, 'Ann'),
  (4, 'Dee');

INSERT INTO accounts (id, owner, kind) VALUES
  (10, 1, 'savings'),
  (11, 1, 'savings'),
  (12, 2, 'checking');
