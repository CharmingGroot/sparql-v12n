-- Customer 3 has no orders, note 2 no customer, and shipment 3 has no
-- line with its columns swapped.
INSERT INTO regions VALUES
  ('n', 'North'),
  ('s', 'South');

INSERT INTO customers VALUES
  (1, 'kim', 'n'),
  (2, 'lee', 's'),
  (3, 'max', 'n');

INSERT INTO orders VALUES
  (1, 1, 10),
  (2, 1, 20),
  (3, 2, 30),
  (4, 2, 40);

INSERT INTO notes VALUES
  (1, 1,    'call'),
  (2, NULL, 'misc'),
  (3, 3,    'visit');

INSERT INTO lines VALUES
  (1, 1, 'pen'),
  (1, 2, 'ink'),
  (2, 1, 'cap'),
  (3, 1, 'pad');

INSERT INTO shipments VALUES
  (1, 1, 1),
  (2, 1, 2),
  (3, 3, 1);

INSERT INTO refunds VALUES
  (1, 2),
  (2, 4);

INSERT INTO employees VALUES
  (1, 'boss', 1),
  (2, 'ann',  1),
  (3, 'bob',  2);
