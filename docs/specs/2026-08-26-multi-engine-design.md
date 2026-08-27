# Multi-engine support: SQLite and MySQL

Date: 2026-08-26
Status: implemented

Slate speaks to one engine. This adds two more without letting either of them
be visible above `src/db/`.

The 2026-08-17 design already anticipated this in §4.1, and hard rule 4 of
AGENTS.md carried its conclusion: everything crossing out of the database
boundary is a rendered `String`, and no driver trait exists. The first half of
that rule is why this change is tractable at all. The second half was written
when a second engine was hypothetical; §9 below records what replaces it.

---

## 1. What is being built

Postgres, MySQL and SQLite, at parity:

- connect, with per-engine credentials and transport
- run the user's SQL verbatim, with results, timings, byte counts and type tags
- the explorer tree: schemas, relations, routines
- the Structure tab: columns, indexes, constraints
- in-grid editing, gated by the same primary-key rule for all three

Delivered in two passes. Pass 1 is the module split plus SQLite end to end.
Pass 2 is MySQL into a boundary that by then already exists. SQLite goes first
deliberately: it is the engine whose shape least resembles Postgres — no host,
no port, no user, no password, no TLS, no schemas in the Postgres sense — so it
forces the boundary to be honest before MySQL, which is structurally close
enough to Postgres to let a bad boundary pass unnoticed.

## 2. Module shape

`src/db.rs` becomes `src/db/`:

| file | holds |
| --- | --- |
| `db/mod.rs` | the vocabulary that crosses out, the `Connection` enum, dispatch |
| `db/postgres.rs` | today's code, moved, behaviour unchanged |
| `db/sqlite.rs` | pass 1 |
| `db/mysql.rs` | pass 2 |

`mod.rs` owns `QueryResult`, `Column`, `Cell`, `Catalog`, `Schema`, `Relation`,
`RelationKind`, `Routine`, `RoutineKind`, `Structure`, `ColumnDefinition`,
`NamedDefinition`, `EditTarget`, `DbError`, `ConnectionConfig`, `ServerConfig`
and `Engine`. None of those types change meaning; they gain implementations.

```rust
pub enum Connection {
    Postgres(postgres::Connection),
    MySql(mysql::Connection),
    Sqlite(sqlite::Connection),
}
```

Each arm owns its own `Arc<Mutex<…>>`, so a profile still serialises its own
queries and nothing else. `query`, `catalog` and `structure` match three ways.

**Why an enum and not a trait.** A trait is an open extension point: it invites
a fourth engine, then a fifth, then a plugin surface, and every shared decision
becomes a default method nobody reads. An enum has three arms the compiler
forces every match to enumerate, and adding a fourth is a visible edit to every
place that has an opinion. The Conventions section of AGENTS.md bans
speculative abstraction; three concrete arms are not speculative.

## 3. Connection configuration

```rust
pub enum ConnectionConfig {
    Postgres(ServerConfig),
    MySql(ServerConfig),
    Sqlite { path: String },
}
```

`ServerConfig` is today's `ConnectionConfig` verbatim: host, port, database,
user, password, sslmode, root certificate. SQLite has none of those. Giving it
a `ServerConfig` with six permanently-empty fields would put six dead inputs in
the connection form and six dead keys in `profiles.toml`, and would make
"blank host" a state the code has to keep deciding is fine.

`ConnectionConfig::from_url` dispatches on scheme: `postgres://` and
`postgresql://` as today, `mysql://`, and `sqlite://` or `file:` for a path.
An unrecognised scheme is refused by name.

`endpoint()` stays, and answers per engine: `host:port` for the two servers,
the file path for SQLite. It exists so errors and window titles can name what
was being talked to.

## 4. Per-engine behaviour

### 4.1 Postgres

Unchanged. The simple query protocol still returns every value pre-formatted by
the server, types are still learned by describing the statement after it ran,
and PostGIS geometry is still rendered as WKT. Moving the file must not change
a single observable behaviour, and the existing test suite is the check.

### 4.2 SQLite

**Driver.** `rusqlite`, `default-features = false`, features `bundled`,
`column_metadata`, `column_decltype`. `bundled` compiles SQLite from source
rather than linking whatever macOS shipped, so the feature set and version are
Slate's own rather than the OS's. `default-features = false` is required
because rusqlite 0.40's defaults include `ffi-sqlite-wasm-rs`, which is not
wanted in a native build. No tokio.

**Opening.** `open_with_flags` with `SQLITE_OPEN_READ_WRITE`, and deliberately
**without** `SQLITE_OPEN_CREATE`. `SQLITE_OPEN_URI` is off too: the path is
resolved out of the URL before the driver sees it, so leaving URI parsing on
would make a path containing `?` mean something other than itself — and one of
the parameters it would then honour is `mode=rwc`, which puts the file creation
straight back.

A mistyped path must be an error that names the path, not a silently created
empty database that then reports an empty catalog as though the file were
simply new.

**Statements.** Split with `rusqlite::Batch`, which uses SQLite's own
prepare-tail rather than a second parser. `sql::Buffer::parse` was the fallback
and is not needed: a statement SQLite accepts and Slate's tree-sitter grammar
does not would otherwise have become a statement Slate refused to send.

The pieces run in order and the last result set wins, the same rule Postgres
follows. But SQLite commits each statement on its own, where one Postgres
submission is one implicit transaction — so a **generated** multi-row batch is
bracketed with `BEGIN` and `COMMIT`. Those brackets go into the statement text
the user can read, edit and undo, never around it invisibly, and
`sql::is_generated_update` learns the shape rather than trusting it: it refuses
a transaction it cannot see closed, and still scans inside for anything
destructive.

**Values.** From `ValueRef`: `Null` is `None`, integers and reals go through
`Display`, `Text` is decoded as UTF-8 and raises the existing non-UTF-8 error
naming the column, and `Blob` is rendered `x'…'` — SQLite's own literal syntax,
so what the grid shows is what SQLite would accept back.

**Types.** `Column::decl_type()`, which is the declared type for a real column
and `None` for an expression. Absent rather than guessed, matching the
Postgres rule: the storage class of the first non-null value is not the
column's type and must not be presented as one.

**Catalog.** `PRAGMA database_list` gives the schemas: `main`, `temp`, and any
`ATTACH`ed database, under SQLite's own names for them. Per schema,
`sqlite_master` gives tables and views, excluding `sqlite_%`. Routines are
always empty — SQLite has none, and an empty list is the truthful answer.

**Structure.** `PRAGMA table_info` for columns, `PRAGMA index_list` joined to
`sqlite_master.sql` for index definitions, and `PRAGMA foreign_key_list` plus
`table_info`'s `pk` for constraints. SQLite has no constraint catalog, so these
are reconstructed into the same `name: definition` shape the other engines
report directly. `CHECK` constraints are left out: SQLite keeps them only inside
the `CREATE TABLE` text, and parsing DDL to show it back is a worse trade than
not showing it.

Nullability needs one correction the pragma does not make. An
`INTEGER PRIMARY KEY` is the rowid under another name and cannot hold a null,
but `table_info` reports `notnull = 0` for it. Every *other* kind of primary key
column in SQLite genuinely can hold one — a real quirk, and one the query is
careful to preserve while fixing the rowid case.

**Edit target.** `Statement::columns_with_metadata()` gives `database_name`,
`table_name` and `origin_name` per column — the provenance the Postgres probe
buys with a round trip. Sole-table and whole-key rules are unchanged: a result
set reading two tables is not editable, and a result set missing any part of
the primary key is not editable. The key comes from `table_info`'s `pk`
column. A table with no declared primary key is not editable, even though it
has a rowid, because the rowid is not in the result set unless the user asked
for it.

### 4.3 MySQL

**Driver.** `mysql` 28 (rust-mysql-simple), blocking, no tokio anywhere in its
dependency tree. TLS via its `rustls-tls-ring` feature, which selects the
`ring` provider — the same one already linked through gpui. `rustls-tls` would
select `aws-lc-rs` and build a second crypto library; that is the same hazard
AGENTS.md already documents for the Postgres path.

**Trust store divergence, accepted knowingly.** The driver performs its own
certificate verification, so `tls.rs`'s custom `rustls` verifiers cannot be
reused. Under `rustls-tls-ring` the driver verifies against `webpki-roots`,
Mozilla's bundled list, while the Postgres path verifies against the macOS
Keychain via `rustls-native-certs`. A corporate root installed only in the
Keychain will therefore pass `verify-full` on Postgres and fail it on MySQL.
This is a documented divergence, not a silent weakening: it fails loudly and
by name, which is what hard rule 7 requires. The upgrade path, if it ever
matters, is to export the platform roots with `rustls-native-certs` — already
a dependency — and hand them to the driver as a root certificate.

**sslmode.** Slate's five rungs map onto `SslOpts`:

| mode | `SslOpts` |
| --- | --- |
| `disable` | none |
| `prefer` | attempt with `require`'s options; on failure, retry with none |
| `require` | `danger_accept_invalid_certs`, `danger_skip_domain_validation` |
| `verify-ca` | root certificate, `danger_skip_domain_validation` |
| `verify-full` | root certificate, nothing skipped |

`prefer` is two connection attempts because the driver cannot express
"encrypt if you can". That is exactly what libpq's `prefer` does, and it is the
only rung where a weaker connection is reached — which is what the word means.

**Values.** From `mysql::Value`. `NULL` is `None`; `Bytes` is decoded as UTF-8
for a text column and rendered as hex for a binary one, decided by the column's
character set rather than by guessing at the content; dates and times are
formatted the way the server would have.

**Types.** `Column::column_type()` mapped to the server's own spellings, with
`flags()` consulted to separate `blob` from `text`, `varbinary` from `varchar`,
and to mark `unsigned`.

**Catalog.** `information_schema.TABLES` for relations, `BASE TABLE` mapping to
`Table` and `VIEW` to `View`, with `information_schema`, `mysql`,
`performance_schema` and `sys` hidden — the four schemas no user wrote.
`information_schema.ROUTINES` for functions and procedures, with
`ROUTINE_DEFINITION` as the body and `PARAMETERS` for the identity arguments.

**Structure.** `information_schema.COLUMNS` for columns, `STATISTICS` for
indexes, `TABLE_CONSTRAINTS` joined to `KEY_COLUMN_USAGE` for constraints. A
column's default reports `AUTO_INCREMENT` or the generation expression where
there is one, because `COLUMN_DEFAULT` alone says "no default" for exactly the
two cases where the server supplies the value itself.

`CHECK` is left out here too, for a different reason: its clause lives in
`information_schema.CHECK_CONSTRAINTS`, which MySQL only grew in 8.0.16, and a
Structure tab that fails wholesale against an older server is worse than one
showing the constraints every server has.

Every aggregate in these queries is grouped, `only_full_group_by` being on by
default since 5.7 — a foreign key's `REFERENCED_TABLE_NAME` beside a
`GROUP_CONCAT` is rejected outright without it.

**Edit target.** `Column::org_table_str()` and `schema_str()` give the sole
table; `org_name_str()` gives the real column name behind an alias. The key
comes from `STATISTICS` where `INDEX_NAME = 'PRIMARY'`. Same rules as the other
two engines.

## 5. Generated SQL

Slate writes SQL in exactly three places: `explorer::preview_sql`, the
`ORDER BY` splice in `sql::with_order_by`, and `sql::update_row`. They ask the
engine three questions, and `Engine` answers them.

- **Identifier quoting.** Postgres and SQLite double-quote and double an
  embedded `"`. MySQL backtick-quotes and doubles an embedded backtick.
- **Qualification.** `quote(schema).quote(table)` on all three. SQLite accepts
  `"main"."t"`, MySQL accepts `` `db`.`t` ``. One implementation.
- **Literal quoting.** All three double an embedded `'`. MySQL additionally
  treats `\` as an escape unless `NO_BACKSLASH_ESCAPES` is set, so it also
  doubles backslashes.

That is two match arms, not a dialect layer. There is no `Dialect` type.

**Assignment casts.** `update_row` writes every value as a quoted literal and
lets the server coerce it, which is what Postgres does with assignment casts.
MySQL coerces the same way. SQLite applies column type affinity on write, so
`'123'` into an `INTEGER` column stores the integer. All three therefore hold
without client-side casting.

**Parse risk.** `tree-sitter-sequel` is one grammar for all three engines. It
may not parse MySQL backtick identifiers. If it does not, `with_order_by`
returns `None` and header-click sorting is unavailable on MySQL — the existing
honest failure mode, and the reason the function refuses rather than guessing.
`is_generated_update` is unaffected, because Slate generates that statement and
therefore controls its syntax.

## 6. Connection form and stored profiles

**Form.** `ConnectionForm` gains an `Engine` and a `path` input. A three-chip
row sits above the fields, built the same way the `sslmode` chips already are.
The field set swaps: SQLite shows a file path and a display name and nothing
else; MySQL and Postgres show host, port, database, user, password and the
`sslmode` chips. The URL box stays where it is and dispatches on scheme, still
filling the fields rather than replacing them — the fields remain the record of
truth.

**Profiles.** `StoredProfile` gains `engine: Option<String>` and
`path: Option<String>`, both `#[serde(default)]`, both inserted **before**
`open_objects`. TOML cannot emit a scalar after a table, which store.rs already
records as load-bearing, and `open_objects` is the table. An absent `engine`
reads as Postgres, so every profile written before this change loads untouched
and keeps working.

The Keychain is keyed by opaque profile id and needs no change. A SQLite
profile stores no password.

**Environment bootstrap.** `PG*` continues to configure a Postgres profile at
startup and is not generalised. Slate is a generic client, not a generic
environment reader, and inventing `SLATE_ENGINE` for a path nobody asked for
would be building ahead of the need.

## 7. Development databases

`compose.yaml` gains a `mysql` service beside the Postgres one: `mysql:8.4`,
`${SLATE_MYSQL_PORT:-53306}:3306`, an init directory at `dev/mysql/init/`, a
`mysqladmin ping` healthcheck and a named volume, mirroring the existing
service in every respect.

SQLite gets no container, because a container for a file is theatre.
`dev/sqlite/001-slate-demo.sql` plus a documented
`sqlite3 dev/slate_dev.db < dev/sqlite/001-slate-demo.sql` is the whole of it;
macOS ships `sqlite3`.

All three seeds define the same demo objects — `accounts`, `locations`,
`account_overview`, `account_label`, a composite-key table and a table with no
primary key — so the `live_` tests read the same assertions against each
engine, and a divergence between engines is visible as a test difference rather
than a seed difference.

## 8. Testing

Every engine module carries the same two layers.

**Unit tests, no server**: value rendering per storage class, type-name
mapping, catalog and structure assembly from synthetic `QueryResult`s,
edit-target resolution from synthetic provenance, identifier and literal
quoting per engine, and URL parsing per scheme. These are the tests that catch
a regression without anything running.

**`#[ignore]`d `live_` tests**, mirroring the existing Postgres suite: query
round trip, multi-statement shape, structure round trip, catalog round trip,
and the five edit-target cases (single table by key, aliased and computed
columns, join, missing key, composite key). Configured from `PG*` as today,
`SLATE_MYSQL_URL`, and `SLATE_SQLITE_PATH`.

## 9. AGENTS.md hard rule 4

Rule 4 currently reads, in part, "This is the only concession to a future
second engine — do not add a driver trait." It is rewritten in place, the way
rule 1 was already rewritten by the in-grid-editing work, to say what it now
means:

> Driver types do not reach the UI layer. The grid receives rendered strings
> and type tags, never a `postgres::Row`, a `mysql::Value` or a
> `rusqlite::ValueRef`. Engine dispatch is a closed enum inside `src/db/` and
> stops there — no trait, no plugin surface, and no code above `src/db/` that
> branches on which engine is connected.

The reasoning is unchanged and is why the rule survives at all: a UI that knows
which engine it is talking to grows an engine-shaped special case in every
view, and those are the special cases nobody ever removes.

## 10. Out of scope

- **Writing `NULL` from the grid.** `sql::update_row` writes every value as a
  quoted literal and cannot express `NULL` for Postgres either. Unchanged here,
  and not made worse.
- **`CHECK` constraints** in the Structure tab, on MySQL and SQLite. Reasons
  above; Postgres still shows them, because Postgres will simply state them.
- **Geometry outside PostGIS.** MySQL has a `GEOMETRY` type; rendering it is a
  separate decision with its own cost, and nobody has asked.
- **`ATTACH` from the UI.** An attached database appears in the tree if the
  file attaches it; Slate does not offer to attach one.
- **MySQL triggers and events.** Not in the explorer for any engine today.
- **A row cap on user queries.** Deferred, deliberately, by an earlier
  decision.
