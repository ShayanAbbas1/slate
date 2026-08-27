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

-- Geometry columns (`point`, `boundary`) are PostGIS-only; MySQL geometry
-- support is out of scope (see docs/specs/2026-08-26-multi-engine-design.md §10).
CREATE TABLE locations (
    id INT PRIMARY KEY,
    name TEXT NOT NULL
);

INSERT INTO locations (id, name) VALUES
    (1, 'San Francisco'),
    (2, 'Null Island');

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

DELIMITER ;
