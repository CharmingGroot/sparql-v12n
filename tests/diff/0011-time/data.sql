INSERT INTO shifts (id, starts) VALUES
  (1, '00:00:00'),
  (2, '24:00:00'),
  (3, '12:30:00.5');
INSERT INTO alarms (at, name) VALUES
  ('00:00:00', 'midnight'),
  ('24:00:00', 'end of day'),
  ('12:30:00.5', 'noon');
