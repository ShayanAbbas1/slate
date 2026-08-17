CREATE EXTENSION IF NOT EXISTS postgis;

CREATE TYPE account_plan AS ENUM ('free', 'team', 'enterprise');

CREATE TABLE accounts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    external_id uuid NOT NULL UNIQUE,
    name text NOT NULL,
    email text,
    plan account_plan NOT NULL,
    balance numeric(14, 2) NOT NULL,
    active boolean NOT NULL,
    tags text[] NOT NULL,
    metadata jsonb NOT NULL,
    created_at timestamptz NOT NULL
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
        true,
        ARRAY['founder', 'priority'],
        '{"timezone":"Europe/London","features":{"audit":true,"seats":250}}',
        '2024-01-15 09:30:00+00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000002',
        'Grace Hopper',
        'grace@example.test',
        'team',
        8192.00,
        true,
        ARRAY['compiler', 'navy'],
        '{"timezone":"America/New_York","languages":["COBOL","English"]}',
        '2024-02-29 12:00:00+00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000003',
        'Edsger Dijkstra',
        NULL,
        'free',
        -0.01,
        false,
        ARRAY[]::text[],
        '{"note":"Simplicity is prerequisite for reliability."}',
        '2024-03-10 18:45:12.123456+00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000004',
        '李小龍',
        'bruce.lee@example.test',
        'team',
        42.42,
        true,
        ARRAY['unicode', '香港'],
        '{"display_name":"李小龍","emoji":"🐉","rtl":"مرحبا"}',
        '2024-04-01 00:00:00+00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000005',
        'Quotes ''n'' Backslashes \\',
        'escaping@example.test',
        'free',
        0.00,
        true,
        ARRAY['quotes', 'backslash'],
        '{"sql":"SELECT ''not a delimiter;'';","path":"C:\\\\demo\\\\file"}',
        '2024-05-05 05:05:05+00'
    );

CREATE TABLE measurements (
    id bigint PRIMARY KEY,
    recorded_at timestamptz NOT NULL,
    sensor text NOT NULL,
    temperature_c double precision,
    pressure_kpa numeric(8, 3),
    healthy boolean NOT NULL,
    samples integer[] NOT NULL,
    payload jsonb NOT NULL
);

INSERT INTO measurements
SELECT
    sample,
    '2025-01-01 00:00:00+00'::timestamptz + sample * interval '15 seconds',
    'sensor-' || lpad(((sample - 1) % 24 + 1)::text, 2, '0'),
    CASE WHEN sample % 97 = 0 THEN NULL ELSE 18.0 + (sample % 150) / 10.0 END,
    98.000 + (sample % 700) / 1000.0,
    sample % 113 <> 0,
    ARRAY[sample % 10, sample % 20, sample % 30],
    jsonb_build_object(
        'sequence', sample,
        'firmware', 'v' || (1 + sample % 3) || '.' || sample % 10,
        'flags', jsonb_build_array(sample % 2 = 0, sample % 5 = 0)
    )
FROM generate_series(1, 5000) AS sample;

CREATE TABLE documents (
    id integer PRIMARY KEY,
    title text NOT NULL,
    body text,
    document jsonb,
    binary_value bytea
);

INSERT INTO documents VALUES
    (
        1,
        'Multiline text',
        E'first line\nsecond line\nthird line; with a semicolon',
        '{"kind":"short","nested":{"null_value":null}}',
        decode('00010203feff', 'hex')
    ),
    (
        2,
        'Large values',
        repeat('Slate keeps the complete value while the grid clips visually. ', 2048),
        jsonb_build_object(
            'kind', 'large',
            'values', (
                SELECT jsonb_agg(jsonb_build_object('index', value, 'square', value * value))
                FROM generate_series(1, 500) AS value
            )
        ),
        decode(repeat('deadbeef', 4096), 'hex')
    );

CREATE TABLE locations (
    id integer PRIMARY KEY,
    name text NOT NULL,
    point geometry(Point, 4326),
    boundary geometry(Polygon, 4326)
);

INSERT INTO locations VALUES
    (
        1,
        'San Francisco',
        ST_SetSRID(ST_MakePoint(-122.4194, 37.7749), 4326),
        ST_GeomFromText(
            'POLYGON((-122.52 37.70,-122.35 37.70,-122.35 37.83,-122.52 37.83,-122.52 37.70))',
            4326
        )
    ),
    (
        2,
        'Null Island',
        ST_SetSRID(ST_MakePoint(0, 0), 4326),
        NULL
    );

CREATE VIEW account_overview AS
SELECT
    plan,
    count(*) AS accounts,
    sum(balance) AS total_balance,
    count(*) FILTER (WHERE active) AS active_accounts
FROM accounts
GROUP BY plan;
