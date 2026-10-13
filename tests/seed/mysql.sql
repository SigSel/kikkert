DROP DATABASE IF EXISTS shop;
CREATE DATABASE shop CHARACTER SET utf8mb4;
USE shop;
CREATE TABLE customers (
  id int PRIMARY KEY,
  email varchar(255),
  name text NOT NULL,
  active boolean NOT NULL DEFAULT true,
  balance decimal(10,2),
  created_at datetime NOT NULL,
  note text,
  token char(36),
  payload varbinary(16),
  meta json
);
INSERT INTO customers VALUES
 (1,'ada@example.com','Ada Lovelace',true,1234.50,'2024-01-15 09:30:00',NULL,'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11',0xDEADBEEF,'{"tier": "gold"}'),
 (2,'bob@example.com','Bob O''Brien',false,0.00,'2024-02-01 12:00:00','50%_off!',NULL,NULL,NULL),
 (3,'ada@example.com','Ada Duplicate',true,NULL,'2024-03-10 18:45:10',NULL,NULL,NULL,NULL),
 (4,NULL,'Blåbær Ærlig',true,-5.25,'2024-03-11 00:00:00','unicode ✓',NULL,NULL,NULL),
 (5,'carol@example.com','Carol',true,99.99,'2024-04-01 08:00:00','back\\slash',NULL,NULL,NULL),
 (6,'bob@example.com','Bob Again',true,10.00,'2024-04-02 08:00:00',NULL,NULL,NULL,NULL);
CREATE INDEX customers_email_idx ON customers (email);
CREATE TABLE orders (
  id int PRIMARY KEY,
  customer_id int NOT NULL,
  total decimal(10,2) NOT NULL,
  CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES customers(id)
);
INSERT INTO orders VALUES (1,1,10.00),(2,1,20.00),(3,2,5.50);
CREATE VIEW active_customers AS SELECT id, name FROM customers WHERE active;
CREATE TABLE empty_table (id int PRIMARY KEY, label text);
CREATE TABLE `Odd ``Name``` (`select` int PRIMARY KEY, `two words` text);
INSERT INTO `Odd ``Name``` VALUES (1, 'quoted');
