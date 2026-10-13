IF DB_ID('rowboat_test') IS NOT NULL
BEGIN
  ALTER DATABASE rowboat_test SET SINGLE_USER WITH ROLLBACK IMMEDIATE;
  DROP DATABASE rowboat_test;
END
GO
CREATE DATABASE rowboat_test;
GO
USE rowboat_test;
GO
CREATE SCHEMA shop;
GO
CREATE TABLE shop.customers (
  id int PRIMARY KEY,
  email varchar(255),
  name nvarchar(200) NOT NULL,
  active bit NOT NULL DEFAULT 1,
  balance decimal(10,2),
  created_at datetime2(0) NOT NULL,
  note nvarchar(max),
  token uniqueidentifier,
  payload varbinary(16),
  meta nvarchar(max)
);
INSERT INTO shop.customers VALUES
 (1,'ada@example.com',N'Ada Lovelace',1,1234.50,'2024-01-15 09:30:00',NULL,'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11',0xDEADBEEF,N'{"tier": "gold"}'),
 (2,'bob@example.com',N'Bob O''Brien',0,0.00,'2024-02-01 12:00:00',N'50%_off!',NULL,NULL,NULL),
 (3,'ada@example.com',N'Ada Duplicate',1,NULL,'2024-03-10 18:45:10',NULL,NULL,NULL,NULL),
 (4,NULL,N'Blåbær Ærlig',1,-5.25,'2024-03-11 00:00:00',N'unicode ✓',NULL,NULL,NULL),
 (5,'carol@example.com',N'Carol',1,99.99,'2024-04-01 08:00:00',N'back\slash',NULL,NULL,NULL),
 (6,'bob@example.com',N'Bob Again',1,10.00,'2024-04-02 08:00:00',NULL,NULL,NULL,NULL);
CREATE INDEX customers_email_idx ON shop.customers (email);
CREATE TABLE shop.orders (
  id int PRIMARY KEY,
  customer_id int NOT NULL CONSTRAINT orders_customer_fk REFERENCES shop.customers(id),
  total decimal(10,2) NOT NULL
);
INSERT INTO shop.orders VALUES (1,1,10.00),(2,1,20.00),(3,2,5.50);
GO
CREATE VIEW shop.active_customers AS SELECT id, name FROM shop.customers WHERE active = 1;
GO
CREATE TABLE shop.empty_table (id int PRIMARY KEY, label nvarchar(50));
CREATE TABLE shop.[Odd "Name"] ([select] int PRIMARY KEY, [two words] nvarchar(50));
INSERT INTO shop.[Odd "Name"] VALUES (1, N'quoted');
GO
