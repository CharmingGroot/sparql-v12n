INSERT INTO products VALUES
  (1,  'anvil',  'tools', 120, 12.5, 4.5,  true,  '2024-03-01 10:00:00', 'anvil'),
  (2,  'bolt',   'tools', 2,   0.01,  3.0,  true,  '2024-01-15 08:30:00', 'bolt'),
  (3,  'chisel', 'tools', 35,  0.4,  4.5,  false, '2024-02-20 12:00:00', 'chisel'),
  (4,  'drill',  'power', 120, 2.25,  4.8,  true,  '2023-12-31 23:59:59', '드릴'),
  (5,  'epoxy',  NULL,    15,  0.25,  NULL, false, NULL,                  'époxy'),
  (6,  'file',   'tools', 9,   0.3,  3.9,  NULL,  '2024-03-01 10:00:00', 'file'),
  (7,  'glue',   NULL,    NULL, 0.1, 2.5,  true,  '2024-01-15 08:30:00', 'glue'),
  (8,  'hammer', 'tools', 25,  1.5,  4.5,  true,  '2024-02-01 00:00:00', 'hammer'),
  (9,  'inkjet', 'power', 199, 6,  NULL, false, '2024-02-29 09:15:00', 'inkjet'),
  (10, 'jigsaw', 'power', 89,  3.5,  4.1,  true,  NULL,                  '직소'),
  (11, 'knife',  'tools', 15,  0.15,  -0.5, false, '2024-01-01 00:00:00', 'knife'),
  (12, 'level',  'tools', 30,  0.8,  2.2,  true,  '2024-04-10 16:45:00', 'level');

INSERT INTO reviews VALUES
  (1, 1, 5, 'solid'),
  (2, 1, 4, NULL),
  (3, 4, 5, 'loud'),
  (4, 8, 3, 'heavy'),
  (5, 8, 5, NULL),
  (6, 10, 2, 'wobbly');
