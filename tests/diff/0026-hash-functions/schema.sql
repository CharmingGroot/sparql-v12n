-- Hash functions (#106): ASCII, multi-byte UTF-8 letters, a combining
-- character, a character outside the BMP, the empty string and a
-- backslash, as simple literals, xsd:string and language-tagged literals.
CREATE TABLE words (
  id     integer  PRIMARY KEY,
  name   text     NOT NULL
);
