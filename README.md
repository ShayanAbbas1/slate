# Slate

A native macOS Postgres client. Rust, GPUI, no Electron.

A SQL editor that shows results — not a database browser with an editor bolted
on. Keyboard-first, minimal, and built to stay at display refresh rate on real
data.

> **Status: design complete, no code yet.** Nothing here runs. The design is at
> [`docs/specs/2026-08-17-slate-design.md`](docs/specs/2026-08-17-slate-design.md).

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

MIT. `theme.rs` and `motion.rs` derive from
[zeronsh/comet](https://github.com/zeronsh/comet), MIT © Wing.
