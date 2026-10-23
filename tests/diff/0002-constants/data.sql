INSERT INTO readings (id, value, measured_on, device, price) VALUES
  (1, 120, DATE '2024-02-29', '00000000-0000-0000-0000-00000000000a', 12.50),
  (2, 7, DATE '2024-03-01', '00000000-0000-0000-0000-00000000000b', 3.00);

-- Label 11 holds the text '120', the same characters as reading 1's integer.
INSERT INTO labels (id, value) VALUES
  (11, '120'),
  (12, 'abc');
