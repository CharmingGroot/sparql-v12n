-- PostgreSQL keeps time '24:00:00' apart from '00:00:00', while XML Schema
-- 1.1 maps both to the same xsd:time value, whose canonical form is
-- 00:00:00 (#37).
CREATE TABLE shifts (
  id     integer PRIMARY KEY,
  starts time    NOT NULL
);
CREATE TABLE alarms (
  at   time PRIMARY KEY,
  name text NOT NULL
);
