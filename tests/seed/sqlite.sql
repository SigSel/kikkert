CREATE TABLE customers (
  id integer PRIMARY KEY,
  email varchar(255),
  name text NOT NULL,
  active boolean NOT NULL DEFAULT 1,
  balance numeric(10,2),
  created_at datetime NOT NULL,
  note text,
  token text,
  payload blob,
  meta text
);
INSERT INTO customers VALUES
 (1,'ada@example.com','Ada Lovelace',1,1234.50,'2024-01-15 09:30:00',NULL,'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11',x'deadbeef','{"tier": "gold"}'),
 (2,'bob@example.com','Bob O''Brien',0,0.00,'2024-02-01 12:00:00','50%_off!',NULL,NULL,NULL),
 (3,'ada@example.com','Ada Duplicate',1,NULL,'2024-03-10 18:45:10',NULL,NULL,NULL,NULL),
 (4,NULL,'Blåbær Ærlig',1,-5.25,'2024-03-11 00:00:00','unicode ✓',NULL,NULL,NULL),
 (5,'carol@example.com','Carol',1,99.99,'2024-04-01 08:00:00','back\slash',NULL,NULL,NULL),
 (6,'bob@example.com','Bob Again',1,10.00,'2024-04-02 08:00:00',NULL,NULL,NULL,NULL);
CREATE INDEX customers_email_idx ON customers (email);
CREATE TABLE orders (
  id integer PRIMARY KEY,
  customer_id integer NOT NULL REFERENCES customers(id),
  total numeric(10,2) NOT NULL
);
INSERT INTO orders VALUES (1,1,10.00),(2,1,20.00),(3,2,5.50);
CREATE VIEW active_customers AS SELECT id, name FROM customers WHERE active;
CREATE TABLE empty_table (id integer PRIMARY KEY, label text);
CREATE TABLE "Odd ""Name""" ("select" integer PRIMARY KEY, "two words" text);
INSERT INTO "Odd ""Name""" VALUES (1, 'quoted');
