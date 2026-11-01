-- Join elimination on foreign keys (#112): referencing object maps and
-- template joins to a referenced table, with keys that give every row
-- one referenced row and keys that do not.
CREATE TABLE regions (
  code text PRIMARY KEY,
  name text NOT NULL
);

CREATE TABLE customers (
  id     integer PRIMARY KEY,
  name   text    NOT NULL,
  region text    NOT NULL REFERENCES regions (code)
);

CREATE TABLE orders (
  id          integer PRIMARY KEY,
  customer_id integer NOT NULL REFERENCES customers (id),
  amount      integer NOT NULL
);

-- A nullable reference: a note without a customer is about no one.
CREATE TABLE notes (
  id          integer PRIMARY KEY,
  customer_id integer REFERENCES customers (id),
  body        text    NOT NULL
);

CREATE TABLE lines (
  order_id integer NOT NULL REFERENCES orders (id),
  line_no  integer NOT NULL,
  item     text    NOT NULL,
  PRIMARY KEY (order_id, line_no)
);

-- A composite key, which the mapping joins column by column, and once
-- with the columns swapped.
CREATE TABLE shipments (
  id       integer PRIMARY KEY,
  order_id integer NOT NULL,
  line_no  integer NOT NULL,
  FOREIGN KEY (order_id, line_no) REFERENCES lines (order_id, line_no)
);

-- A reference of another type than the referenced column.
CREATE TABLE refunds (
  id       integer PRIMARY KEY,
  order_id bigint  NOT NULL REFERENCES orders (id)
);

-- A table that references itself: the root employee is its own manager.
CREATE TABLE employees (
  id         integer PRIMARY KEY,
  name       text    NOT NULL,
  manager_id integer NOT NULL REFERENCES employees (id)
);
