-- Functions on strings (#103): ASCII and other letters, a combining
-- character, a character outside the BMP, LIKE wildcards and a backslash,
-- and the same values with a language tag.
CREATE TABLE words (
  id     integer  PRIMARY KEY,
  name   text     NOT NULL,
  label  text     NOT NULL
);
