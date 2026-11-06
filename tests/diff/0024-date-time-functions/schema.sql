-- YEAR, MONTH, DAY, HOURS, MINUTES, SECONDS, TIMEZONE and TZ (#105) over
-- timestamp and timestamptz columns: zones on both sides of UTC, a
-- timestamptz whose UTC form is in the year before its written one,
-- fractional seconds, a leap day, a string, which is a type error, and a
-- date, whose functions are rejected. No infinite values: they are plain
-- literals, and the PostgreSQL tests cover them.
CREATE TABLE events (
  id    integer      PRIMARY KEY,
  name  text         NOT NULL,
  ts    timestamp    NOT NULL,
  tz    timestamptz  NOT NULL,
  day   date         NOT NULL
);
