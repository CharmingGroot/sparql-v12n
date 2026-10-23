-- Strings in code point order: 'Bolt' < 'bolt' < 'nut' < 'écrou'.
INSERT INTO items VALUES
  (1, 'Bolt',  5,    5000000000, 0.50,  1.5,  true,
      '2024-01-15 10:00:00', '2024-01-15 10:00:00+00', '2024-01-15', 'bolt', NULL),
  (2, 'bolt',  12,   12,         12.00, 0.25, false,
      '2023-12-31 23:59:59.5', '2024-01-01 08:00:00+09', '2023-12-31', 'small bolt', 1),
  (3, 'nut',   NULL, NULL,       7.25,  '-0', NULL,
      NULL, NULL, NULL, NULL, 1),
  (4, 'écrou', 7,    -3,         NULL,  NULL, true,
      '2024-06-01 00:00:00', '2024-06-01 00:00:00+02', '2024-06-01', 'nut', 2),
  (5, '',      0,    0,          0,     0,    false,
      '2024-01-01 00:00:00', '2023-12-31 23:00:00+00', '2024-01-01', '', 9);

INSERT INTO tags VALUES
  (1, 'metal'),
  (2, 'metal'),
  (2, 'small'),
  (4, 'metal');
