CREATE TABLE accounts (
    id INTEGER PRIMARY KEY,
    external_id TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    email TEXT,
    plan TEXT NOT NULL CHECK (plan IN ('free', 'team', 'enterprise')),
    balance NUMERIC NOT NULL,
    active BOOLEAN NOT NULL,
    tags TEXT NOT NULL,
    metadata TEXT NOT NULL,
    created_at TEXT NOT NULL
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
        '["founder","priority"]',
        '{"timezone":"Europe/London","features":{"audit":true,"seats":250}}',
        '2024-01-15 09:30:00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000002',
        'Grace Hopper',
        'grace@example.test',
        'team',
        8192.00,
        TRUE,
        '["compiler","navy"]',
        '{"timezone":"America/New_York","languages":["COBOL","English"]}',
        '2024-02-29 12:00:00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000003',
        'Edsger Dijkstra',
        NULL,
        'free',
        -0.01,
        FALSE,
        '[]',
        '{"note":"Simplicity is prerequisite for reliability."}',
        '2024-03-10 18:45:12.123456'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000004',
        '李小龍',
        'bruce.lee@example.test',
        'team',
        42.42,
        TRUE,
        '["unicode","香港"]',
        '{"display_name":"李小龍","emoji":"🐉","rtl":"مرحبا"}',
        '2024-04-01 00:00:00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000005',
        'Quotes ''n'' Backslashes \\',
        'escaping@example.test',
        'free',
        0.00,
        TRUE,
        '["quotes","backslash"]',
        '{"sql":"SELECT ''not a delimiter;'';","path":"C:\\\\demo\\\\file"}',
        '2024-05-05 05:05:05'
    );

-- Geometry columns (`point`, `boundary`) are PostGIS-only; SQLite has no
-- built-in geometry type (SpatiaLite is a separate extension, out of scope).
CREATE TABLE locations (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);

INSERT INTO locations (id, name) VALUES
    (1, 'San Francisco'),
    (2, 'Null Island');

CREATE VIEW account_overview AS
SELECT
    plan,
    count(*) AS accounts,
    sum(balance) AS total_balance,
    count(CASE WHEN active THEN 1 END) AS active_accounts
FROM accounts
GROUP BY plan;

-- account_label is a Postgres/MySQL stored function; SQLite has no stored
-- functions, so there is nothing to port here.
