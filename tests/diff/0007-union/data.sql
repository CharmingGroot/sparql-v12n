INSERT INTO customers VALUES
  (1, 'Ann', 'Oslo'),
  (2, 'Bo',  NULL),
  (3, 'Cy',  'Rome');

INSERT INTO wholesalers VALUES
  ('w1', 'Dex', 'http://dex.example.org/'),
  ('w2', 'Eli', NULL);

-- customer/1 is also customer 1's IRI, with the same name: the same
-- triples as customer 1's.
INSERT INTO partners VALUES
  ('http://example.org/customer/1', 'Ann'),
  ('http://example.org/partner/fox', 'Fox'),
  ('http://example.org/partner/fox', 'Fox');

INSERT INTO orders VALUES
  (10, 1,    100, 'open'),
  (11, 1,    250, 'shipped'),
  (12, 3,    75,  'open'),
  (13, NULL, 40,  'open');

INSERT INTO notes VALUES
  (1, 10, 'rush',      'en'),
  (2, 10, 'urgente',   'es'),
  (3, 12, 'gift',      'en');
