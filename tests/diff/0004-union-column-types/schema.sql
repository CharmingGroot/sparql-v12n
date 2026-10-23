-- One variable over columns of different SQL types in different triples
-- maps (#24): the same predicates from integer, text and numeric columns,
-- and subjects from an integer and a text key.
CREATE TABLE gadgets (
  id     integer      PRIMARY KEY,
  weight integer      NOT NULL,
  tag    integer      NOT NULL,
  price  numeric(6,1) NOT NULL
);

CREATE TABLE widgets (
  code   text    PRIMARY KEY,
  weight text    NOT NULL,
  tag    text    NOT NULL,
  price  integer NOT NULL
);
