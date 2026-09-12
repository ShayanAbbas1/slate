# Slate

A native macOS database client, speaking Postgres, MySQL and SQLite. Rust,
GPUI, no Electron.

A data browser and a SQL editor as equals: open a table and page through it,
or write the exact statement you mean. Keyboard-first, minimal, and built to
stay at display refresh rate on real data.

> **Status: early development.** Everything below under "What works" runs today.

## Development databases

The repository includes a disposable database per engine, all carrying the same
demo objects with deterministic data for editor, result-grid and large-value
testing: enum, UUID, numeric, array, JSON, `NULL`, Unicode, binary, large text
and 5,000 measurement rows. Geometry is Postgres-only — PostGIS has no
equivalent in the other two, and Slate does not pretend otherwise.

```sh
docker compose up -d                                       # postgres + mysql
sqlite3 dev/slate_dev.db < dev/sqlite/001-slate-demo.sql    # a file, not a service
cargo run
```

Pick the engine in the connection form, then paste a URL or fill in the fields:

```text
postgresql://slate:slate@127.0.0.1:55432/slate_dev
mysql://slate:slate@127.0.0.1:53306/slate_dev
/absolute/path/to/slate/dev/slate_dev.db
```

`PG*` environment variables still configure a Postgres profile at startup and
are not generalised — Slate is a generic client, not a generic environment
reader, and the other two engines have no such convention to read.
`PGPASSWORD` is used for that session and nothing more: a profile the
environment made gets no Keychain entry, because a variable set in a shell is
not a credential anyone asked Slate to keep.

```sh
PGHOST=127.0.0.1 PGPORT=55432 PGDATABASE=slate_dev \
PGUSER=slate PGPASSWORD=slate cargo run
```

The official PostGIS image is currently `amd64`-only. Docker Desktop runs it
under emulation on Apple Silicon; `compose.yaml` declares that platform
explicitly rather than emitting a misleading mismatch warning.

Initialization runs only when Docker creates the data volume, so reset it after
changing a seed file. The MySQL container reports itself healthy even when its
init script failed, so check the row count rather than the status:

```sh
docker compose down --volumes && docker compose up -d
rm -f dev/slate_dev.db && sqlite3 dev/slate_dev.db < dev/sqlite/001-slate-demo.sql
```

## What works

- **Postgres, MySQL and SQLite**, behind one interface. Pick the engine on the
  connection form and the fields follow it — a file path for SQLite, host and
  credentials for the other two. No driver type reaches the UI, so the grid,
  the explorer and the editor do not know which engine they are showing.
- **Connection profiles with isolated workspaces.** Switch database and your
  whole set of tabs, tree and history switches with it. Nothing is shared, so a
  buffer written against staging cannot be silently retargeted at production.
  Profiles persist; passwords live in the Keychain.
- **Query editor** with tree-sitter SQL highlighting. `cmd+enter` runs the
  selection, or the statement under the cursor. Errors render inline.
- **Completion from your own schema.** Typing offers the schemas, tables,
  views, routines and columns the connection actually has, plus the keywords
  that carry a statement's shape. After `FROM` it offers relations; after a
  table name or an alias and a dot, that table's columns; inside a string
  literal or a comment, nothing at all.
- **As many query buffers as you want.** `cmd+t` opens another, each with its
  own results, sort state and history of what it ran. An unsaved one persists
  by itself, so a scratch buffer survives a restart without being named.
- **Virtualized result grid** with content-fitted draggable columns and a row
  inspector showing whole values and their types. Table previews carry a
  per-tab row limit and page through the relation a window at a time; a query
  you wrote runs exactly as written, uncapped.
- **Filter rows without writing SQL.** A table preview stacks one bar per
  filter — pick a column, pick an operator, type a value — and the generated
  `SELECT` carries them in its `WHERE`, quoted for the engine you are on.
  Eighteen operators, from `=` through contains, `IN`, `BETWEEN` and a regex
  match; `AND` or `OR` between any two bars, folded the way the stack reads. A
  bar can hold raw SQL instead, which goes in verbatim and is refused if the
  statement it makes is not one readable `SELECT`.
- **Sorting that edits your SQL in front of you.** A header click splices an
  `ORDER BY` into the statement in the buffer — the statement that runs is the
  statement on screen, and you can edit or undo it.
- **In-grid editing.** Click or arrow to a cell, `Enter` to edit, `cmd+c` to copy
  the whole value. Apply writes one `UPDATE` per changed row into the buffer and
  runs it. A cell is editable only when Slate can identify its row by primary
  key; joins, aggregates, views and keyless tables stay read-only and say why.
- **Schema explorer** over schemas, tables, views, functions and procedures, with
  an inline filter and a structure view for columns, indexes and constraints.
- **`cmd+p` and `cmd+shift+p`.** One flat fuzzy list over every table, view,
  routine and saved query the connection has, and one over the verbs that apply
  to what is on screen. Every row runs the same code the buttons do.
- **Cancel, and a statement timeout.** A running query has a Cancel button that
  reaches the statement itself, and each profile can carry a timeout applied at
  connect. What each engine buys with that differs and Slate does not pretend
  otherwise: on MySQL the timeout bounds read-only `SELECT`s only, and SQLite's
  is wall clock rather than work done.
- **Export** the result set in front of you to CSV or JSON. It writes what the
  tab already holds — no second fetch, no generated SQL — and the file extension
  picks the format.
- **Query history**, per profile and appended to on every run — a failed
  statement included, since that is the one worth getting back. Reach it from
  the command palette; recalling a statement appends it to the buffer with the
  cursor on it, so `cmd+enter` sends what you are looking at.
- **`cmd+w`** closes the tab in front. A saved query is listed while its file
  exists, so closing that one is deleting it and it asks first; everything else
  just goes. The exception is the last unsaved buffer, which stays — a profile
  always has somewhere to write.
- **Two themes and a glass one**, cycled with `cmd+shift+t`, and the sidebar,
  the editor and the grid each pick their own font. `cmd+shift+s` folds the
  sidebar away; `cmd+±` and `cmd+0` size the editor.
- **TLS, with libpq's five `sslmode` rungs** — `disable`, `prefer`, `require`,
  `verify-ca`, `verify-full` — selectable per connection and carried through to
  both server engines. `verify-full` checks the certificate against the macOS
  trust store on Postgres, or against a root certificate you name, which
  replaces that store rather than adding to it. A mode is never quietly
  downgraded: ask for encryption and Slate either gets it or tells you which
  certificate failed and why. SQLite has no transport to secure, so it has no
  such setting.

## Installing

There are no downloads. Build it:

```sh
dev/bundle.sh
```

That produces `/Applications/Slate.app` — release build, icon, signature and
all. It is signed ad-hoc unless you give it an identity, which is fine for the
machine that compiled it and no good on any other: there is no Developer ID and
no notarization yet, so this is a build-it-yourself app rather than one to pass
around.

## Planned

SSH tunneling, notarized builds, and a Homebrew tap.

## Not planned

Visual query builders, ER diagrams, migrations. Slate assumes you can write
SQL; it just does not make you write all of it.

**Slate never writes a `DROP` or `TRUNCATE`** — not on request, not by
accident. Generated statements pass a whitelist gate that admits three shapes
and nothing else — an `UPDATE`, a single-row `INSERT`, and a `DELETE` of one row
named by its primary key — so the guarantee is structural rather than a list of
names someone remembered to check. The `DELETE` is generated only from an
explicit ask and shown before it runs, never from a predicate Slate guessed at
and never more than one row per statement; and its shape is read back out of the
parse tree rather than trusted because Slate wrote it, since a gate that trusts
its caller is a comment. On MySQL and SQLite, where each statement commits on its own, a
multi-row edit is bracketed with `BEGIN`/`COMMIT` — written into the buffer
where you can read it, never opened behind your back. A batch that fails part
way is rolled back, and the error says which state the data is in.

## License

MIT. See [NOTICES.md](NOTICES.md) for third-party font licenses.
