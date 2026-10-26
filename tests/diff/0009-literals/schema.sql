-- Literal typing rules (#53): language tags that differ only in case,
-- xsd:string and simple literals, and constants whose datatype a column
-- never yields.
CREATE TABLE words (
  id    integer          PRIMARY KEY,
  word  text             NOT NULL,
  count integer          NOT NULL,
  ratio double precision NOT NULL,
  price numeric(6,2)     NOT NULL,
  ok    boolean          NOT NULL
);
