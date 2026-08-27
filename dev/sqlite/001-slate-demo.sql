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

-- Five thousand rows, so scrolling, sorting and the row-count chip have
-- something to work against rather than a grid that fits on screen.
CREATE TABLE measurements (
    id INTEGER PRIMARY KEY,
    recorded_at TEXT NOT NULL,
    sensor TEXT NOT NULL,
    temperature_c REAL,
    pressure_kpa NUMERIC,
    healthy BOOLEAN NOT NULL,
    samples TEXT NOT NULL,
    payload TEXT NOT NULL
);

WITH RECURSIVE series (sample) AS (
    SELECT 1
    UNION ALL
    SELECT sample + 1 FROM series WHERE sample < 5000
)
INSERT INTO measurements
SELECT
    sample,
    strftime('%Y-%m-%d %H:%M:%f', '2025-01-01 00:00:00', (sample * 15) || ' seconds'),
    'sensor-' || printf('%02d', (sample - 1) % 24 + 1),
    -- A null every 97th row, so the grid's null rendering is reachable by
    -- scrolling rather than only by writing a query for it.
    CASE WHEN sample % 97 = 0 THEN NULL ELSE 18.0 + (sample % 150) / 10.0 END,
    98.000 + (sample % 700) / 1000.0,
    sample % 113 <> 0,
    json_array(sample % 10, sample % 20, sample % 30),
    json_object(
        'sequence', sample,
        'firmware', 'v' || (1 + sample % 3) || '.' || (sample % 10),
        'flags', json_array(sample % 2 = 0, sample % 5 = 0)
    )
FROM series;

-- Values far larger than a cell can show, and values a cell would misread:
-- embedded newlines, an embedded semicolon, a JSON null, and raw bytes. The
-- blob is what exercises the `x'...'` rendering.
CREATE TABLE documents (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    body TEXT,
    document TEXT,
    binary_value BLOB
);

INSERT INTO documents VALUES (
    1,
    'Multiline text',
    -- SQLite has no backslash escapes in a string literal, so the newlines are
    -- built rather than written.
    'first line' || char(10) || 'second line' || char(10) || 'third line; with a semicolon',
    '{"kind":"short","nested":{"null_value":null}}',
    x'00010203FEFF'
);

-- SQLite has no `repeat`, so a run of N copies is `hex(zeroblob(N))` -- 2N
-- zero characters -- with each '00' replaced by the text to repeat.
WITH RECURSIVE series (value) AS (
    SELECT 1
    UNION ALL
    SELECT value + 1 FROM series WHERE value < 500
)
INSERT INTO documents
SELECT
    2,
    'Large values',
    replace(hex(zeroblob(2048)), '00', 'Slate keeps the complete value while the grid clips visually. '),
    json_object(
        'kind', 'large',
        'values', json_group_array(json_object('index', value, 'square', value * value))
    ),
    unhex(replace(hex(zeroblob(4096)), '00', 'deadbeef'))
FROM series;

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
-- `deactivate_account` is a Postgres/MySQL stored procedure, and has nowhere to
-- go here for the same reason.
