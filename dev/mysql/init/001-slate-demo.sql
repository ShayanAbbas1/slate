CREATE TABLE accounts (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    external_id CHAR(36) NOT NULL UNIQUE,
    name TEXT NOT NULL,
    email TEXT,
    plan ENUM('free', 'team', 'enterprise') NOT NULL,
    balance DECIMAL(14, 2) NOT NULL,
    active BOOLEAN NOT NULL,
    tags JSON NOT NULL,
    metadata JSON NOT NULL,
    created_at DATETIME(6) NOT NULL
);

INSERT INTO accounts (
    external_id,
    name,
    email,
    plan,
    balance,
    active,
    tags,
    metadata,
    created_at
) VALUES
    (
        '018f1f6e-7c2a-7000-8000-000000000001',
        'Ada Lovelace',
        'ada@example.test',
        'enterprise',
        125000.50,
        TRUE,
        JSON_ARRAY('founder', 'priority'),
        JSON_OBJECT('timezone', 'Europe/London', 'features', JSON_OBJECT('audit', TRUE, 'seats', 250)),
        '2024-01-15 09:30:00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000002',
        'Grace Hopper',
        'grace@example.test',
        'team',
        8192.00,
        TRUE,
        JSON_ARRAY('compiler', 'navy'),
        JSON_OBJECT('timezone', 'America/New_York', 'languages', JSON_ARRAY('COBOL', 'English')),
        '2024-02-29 12:00:00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000003',
        'Edsger Dijkstra',
        NULL,
        'free',
        -0.01,
        FALSE,
        JSON_ARRAY(),
        JSON_OBJECT('note', 'Simplicity is prerequisite for reliability.'),
        '2024-03-10 18:45:12.123456'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000004',
        '李小龍',
        'bruce.lee@example.test',
        'team',
        42.42,
        TRUE,
        JSON_ARRAY('unicode', '香港'),
        JSON_OBJECT('display_name', '李小龍', 'emoji', '🐉', 'rtl', 'مرحبا'),
        '2024-04-01 00:00:00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000005',
        'Quotes ''n'' Backslashes \\\\',
        'escaping@example.test',
        'free',
        0.00,
        TRUE,
        JSON_ARRAY('quotes', 'backslash'),
        JSON_OBJECT('sql', 'SELECT ''not a delimiter;'';', 'path', 'C:\\\\demo\\\\file'),
        '2024-05-05 05:05:05'
    );

-- Five thousand rows, so scrolling, sorting and the row-count chip have
-- something to work against rather than a grid that fits on screen.
--
-- The recursive CTE needs its ceiling raised first: `cte_max_recursion_depth`
-- defaults to 1,000, and the failure is an error rather than a short table.
SET SESSION cte_max_recursion_depth = 10000;

CREATE TABLE measurements (
    id BIGINT PRIMARY KEY,
    recorded_at DATETIME(6) NOT NULL,
    sensor VARCHAR(32) NOT NULL,
    temperature_c DOUBLE,
    pressure_kpa DECIMAL(8, 3),
    healthy BOOLEAN NOT NULL,
    samples JSON NOT NULL,
    payload JSON NOT NULL
);

-- The `WITH` goes after `INSERT INTO`, which is the only place MySQL
-- accepts one on an `INSERT ... SELECT`.
INSERT INTO measurements
WITH RECURSIVE series (sample) AS (
    SELECT 1
    UNION ALL
    SELECT sample + 1 FROM series WHERE sample < 5000
)
SELECT
    sample,
    TIMESTAMPADD(SECOND, sample * 15, '2025-01-01 00:00:00'),
    CONCAT('sensor-', LPAD((sample - 1) % 24 + 1, 2, '0')),
    -- A null every 97th row, so the grid's null rendering is reachable by
    -- scrolling rather than only by writing a query for it.
    CASE WHEN sample % 97 = 0 THEN NULL ELSE 18.0 + (sample % 150) / 10.0 END,
    98.000 + (sample % 700) / 1000.0,
    sample % 113 <> 0,
    JSON_ARRAY(sample % 10, sample % 20, sample % 30),
    JSON_OBJECT(
        'sequence', sample,
        'firmware', CONCAT('v', 1 + sample % 3, '.', sample % 10),
        'flags', JSON_ARRAY(sample % 2 = 0, sample % 5 = 0)
    )
FROM series;

-- Values far larger than a cell can show, and values a cell would misread:
-- embedded newlines, an embedded semicolon, a JSON null, and raw bytes.
CREATE TABLE documents (
    id INT PRIMARY KEY,
    title TEXT NOT NULL,
    body LONGTEXT,
    document JSON,
    binary_value LONGBLOB
);

INSERT INTO documents VALUES (
    1,
    'Multiline text',
    'first line\nsecond line\nthird line; with a semicolon',
    JSON_OBJECT('kind', 'short', 'nested', JSON_OBJECT('null_value', CAST('null' AS JSON))),
    UNHEX('00010203feff')
);

-- The `WITH` goes after `INSERT INTO`, which is the only place MySQL
-- accepts one on an `INSERT ... SELECT`.
INSERT INTO documents
WITH RECURSIVE series (value) AS (
    SELECT 1
    UNION ALL
    SELECT value + 1 FROM series WHERE value < 500
)
SELECT
    2,
    'Large values',
    REPEAT('Slate keeps the complete value while the grid clips visually. ', 2048),
    JSON_OBJECT(
        'kind', 'large',
        'values', JSON_ARRAYAGG(JSON_OBJECT('index', value, 'square', value * value))
    ),
    UNHEX(REPEAT('deadbeef', 4096))
FROM series;

-- Geometry columns (`point`, `boundary`) are PostGIS-only; MySQL geometry
-- support is out of scope.
CREATE TABLE locations (
    id INT PRIMARY KEY,
    name TEXT NOT NULL
);

INSERT INTO locations (id, name) VALUES
    (1, 'San Francisco'),
    (2, 'Null Island');

-- Keys worth following. `orders` is both ends of the problem at once: a
-- composite primary key, and a single-column foreign key into `accounts`, so
-- the simple case and the parent of the hard case are one table.
CREATE TABLE orders (
    account_id BIGINT NOT NULL,
    number INT NOT NULL,
    placed_at DATETIME(6) NOT NULL,
    total DECIMAL(14, 2) NOT NULL,
    PRIMARY KEY (account_id, number),
    FOREIGN KEY (account_id) REFERENCES accounts (id)
);

INSERT INTO orders VALUES
    (1, 1001, '2024-06-01 10:00:00', 4500.00),
    (1, 1002, '2024-06-08 11:30:00', 125.75),
    (2, 2001, '2024-06-12 16:15:00', 890.10);

-- The composite foreign key, which the catalog has to report as one key over
-- two columns rather than two keys of one column each.
CREATE TABLE order_items (
    id INT PRIMARY KEY,
    order_account_id BIGINT NOT NULL,
    order_number INT NOT NULL,
    description TEXT NOT NULL,
    quantity INT NOT NULL,
    FOREIGN KEY (order_account_id, order_number) REFERENCES orders (account_id, number)
);

INSERT INTO order_items VALUES
    (1, 1, 1001, 'Analytical engine time', 3),
    (2, 1, 1002, 'Punch card stock', 500),
    (3, 2, 2001, 'Compiler seat', 1);

-- MySQL has no schemas inside a database, so the cross-schema case is a second
-- database. InnoDB takes a foreign key across databases, and
-- `information_schema` names the referenced schema either way -- which is the
-- thing the catalog query has to get right.
--
-- The entrypoint grants the app user `slate` rights on MYSQL_DATABASE only, so
-- the second database needs its own grant or the app cannot read what it seeds.
CREATE DATABASE slate_archive;

GRANT ALL PRIVILEGES ON slate_archive.* TO 'slate'@'%';

CREATE TABLE slate_archive.closed_accounts (
    id INT PRIMARY KEY,
    account_id BIGINT NOT NULL,
    closed_at DATETIME(6) NOT NULL,
    FOREIGN KEY (account_id) REFERENCES slate_dev.accounts (id)
);

INSERT INTO slate_archive.closed_accounts VALUES
    (1, 3, '2024-07-01 00:00:00'),
    (2, 5, '2024-07-04 12:00:00');

CREATE VIEW account_overview AS
SELECT
    plan,
    COUNT(*) AS accounts,
    SUM(balance) AS total_balance,
    COUNT(CASE WHEN active THEN 1 END) AS active_accounts
FROM accounts
GROUP BY plan;

-- Routines the explorer can actually show. Extension-owned routines are
-- filtered out of the catalog, so without this the routine surface has
-- nothing to display against this database.
DELIMITER $$

CREATE FUNCTION account_label(account_id BIGINT) RETURNS TEXT
DETERMINISTIC
READS SQL DATA
BEGIN
    DECLARE label TEXT;
    SELECT CONCAT(name, ' (', plan, ')') INTO label
    FROM accounts
    WHERE id = account_id;
    RETURN label;
END$$

CREATE PROCEDURE deactivate_account(account_id BIGINT)
MODIFIES SQL DATA
BEGIN
    UPDATE accounts SET active = FALSE WHERE id = account_id;
END$$

DELIMITER ;
