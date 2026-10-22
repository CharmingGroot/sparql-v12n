-- Example source database: customers and their orders.
CREATE TABLE customers (
  id   integer PRIMARY KEY,
  name text    NOT NULL,
  tier text    NOT NULL
);

CREATE TABLE orders (
  id          integer PRIMARY KEY,
  customer_id integer NOT NULL REFERENCES customers (id),
  amount      integer NOT NULL
);

CREATE INDEX orders_customer_id ON orders (customer_id);
