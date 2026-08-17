# Slate

A native macOS Postgres client. Rust, GPUI, no Electron.

A SQL editor that shows results — not a database browser with an editor bolted
on. Keyboard-first, minimal, and built to stay at display refresh rate on real
data.

> **Status: early development.** Slate can connect and run SQL; the result grid,
> connection form and distribution work are still in progress. The design is at
> [`docs/specs/2026-08-17-slate-design.md`](docs/specs/2026-08-17-slate-design.md).

## Development database

The repository includes a disposable Postgres/PostGIS database with deterministic
data for editor, result-grid and large-value testing.

```sh
docker compose up -d postgres

PGHOST=127.0.0.1 \
PGPORT=55432 \
PGDATABASE=slate_dev \
PGUSER=slate \
PGPASSWORD=slate \
cargo run
```

The seed includes enum, UUID, numeric, array, JSONB, `NULL`, Unicode, binary,
large text, 5,000 measurement rows and PostGIS geometry values.

The official PostGIS image is currently `amd64`-only. Docker Desktop runs it
under emulation on Apple Silicon; `compose.yaml` declares that platform
explicitly rather than emitting a misleading mismatch warning.

Initialization runs only when Docker creates the data volume. Reset it after
changing a seed file:

```sh
docker compose down --volumes
docker compose up -d postgres
```

## Planned for v1

- Connection profiles with isolated workspaces — switch database, switch your
  whole set of tabs, tree and history. Nothing shared.
- Query editor with tree-sitter SQL highlighting. `cmd+enter` runs the selection,
  or the statement under the cursor.
- Virtualized result grid with a configurable row limit and a scrollable value
  inspector for large JSONB and PostGIS values.
- `cmd+p` fuzzy table search, `cmd+shift+p` command palette.
- Schema browsing with an inline filter.
- TLS.

## Not planned

Row editing, visual query builders, ER diagrams, migrations. Slate assumes you
write SQL.

## License

MIT.
