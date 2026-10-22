INSERT INTO customers (id, name, tier) VALUES
  (7, 'Kim', 'VIP'),
  (8, 'Lee', 'REGULAR'),
  (9, 'Park', 'VIP');

INSERT INTO orders (id, customer_id, amount) VALUES
  (1001, 7, 120000),
  (1002, 8, 30000),
  (1003, 9, 87000),
  (1004, 7, 15000);
