INSERT INTO measures (id, name, f, g, n, i, d) VALUES
  (1, 'one',   1.5,         2,     1,     7,  1.50),
  (2, 'two',   '-0',        4,     'NaN', -3, 2.25),
  (3, 'three', 'Infinity',  0.5,   -2,    4,  0.00),
  (4, 'four',  '-Infinity', 3,     'NaN', 12, 4.75),
  (5, 'five',  1e300,       10,    0.25,  0,  -1.25),
  (6, 'six',   0.1,         0.2,   5,     1,  0.30);
