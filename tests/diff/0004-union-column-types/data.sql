-- Gadget 1 and widget '1' have the same subject IRI and the same tag IRI,
-- and weights with the same characters but different datatypes.
INSERT INTO gadgets (id, weight, tag, price) VALUES
  (1, 120, 7, 2.5),
  (2, 7, 8, 4.5);

INSERT INTO widgets (code, weight, tag, price) VALUES
  ('1', '120', '7', 3),
  ('w', 'abc', '9', 2);
