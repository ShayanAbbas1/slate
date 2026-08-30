# Slate — Design

**Date:** 2026-08-17
**Status:** Approved for planning

A native macOS Postgres client written in Rust on GPUI. Fast, minimal, and
keyboard-first — a SQL editor that shows results, not a database browser with an
editor bolted on.

> Slate now speaks MySQL and SQLite as well. §4.1 and the two non-goal rows
> about a second engine are superseded by
> `2026-08-26-multi-engine-design.md`; everything else here still holds.

---

## 1. What Slate is

Slate targets the gap between three unsatisfying options: Java clients with
legacy interfaces, Electron clients that are slow, and polished native clients
that are paywalled. It is a native binary with a hand-designed interface, built
so that interaction stays at display refresh rate under real workloads.

**Design center:** open a connection, write SQL, read results. Everything else is
secondary and most of it is deferred.

### v1 is done when

A developer can install Slate, add a Postgres connection over TLS, write and run
queries with syntax highlighting, read results in a virtualized grid, inspect
large cell values, browse and search the schema, and drive all of it from the
keyboard.

---

## 2. Non-goals

Explicitly out of scope for v1. Each is a deliberate cut, not an oversight.

| Not in v1 | Why |
| --- | --- |
| ~~Row editing / `UPDATE` via grid~~ | Superseded — see `2026-08-23-in-grid-editing-design.md`. The reasoning below still holds for `DELETE`, `INSERT` and the rest of a data-entry surface; only `UPDATE` by primary key is in. |
| Visual filter and join builders | Users who reach for a SQL client write SQL. |
| Foreign-key navigation | Depends on the browser-first model Slate rejects. |
| SSH tunneling | Additive later; the config is shaped for it. |
| ~~Engines other than Postgres~~ | Superseded — see `2026-08-26-multi-engine-design.md`. The rule §4.1 was protecting held and is why the change was a refactor: engine dispatch is a closed enum in `src/db/`, still not a trait. |
| Multiple windows | Single window, single root entity. |
| ~~CSV / Parquet export~~ | Superseded for CSV and JSON — asked for on demand, and it was as cheap as this row predicted: `src/export.rs` renders the result set a tab already holds, so there is no second fetch path and no streaming. Parquet stays out. Clipboard copy is still the one-cell answer. An export writes the rows the tab has: on a relation tab that is what the row-limit chip asked for, and it is not re-read behind the chip's back. |
| Configurable keybindings | Hardcoded in v1. |
| ER diagrams, migrations, seed tooling | Different product. |

---

## 3. Shell architecture

### 3.1 Profiles

**A connection is a profile, not a property of a tab.** The model is Arc's
spaces: each profile owns its own open tabs, its own schema tree, its own query
history, and its own scroll and selection state. **Nothing is shared across
profiles.**

Profiles are switched from a control at the bottom of the sidebar, styled as an
account switcher, and from the command palette.

Rationale: the alternative — one global "current connection" that repoints open
buffers — is how a user runs a staging query against production. When the
connection owns the workspace, a buffer written against one database cannot be
silently retargeted at another, because it does not exist outside its profile.

Accepted consequence: two databases cannot be viewed side by side. Comparing
environments visually is not supported; switching is a keystroke instead.

The only cross-profile state is the profile list itself, which the switcher must
render.

```
AppState
└── profiles: Vec<Profile>            // the only global collection
    └── Profile
        ├── connection: ConnectionConfig
        ├── surfaces: Vec<Surface>    // open tabs, ordered
        ├── active_surface: usize
        ├── schema: SchemaTree        // cached, lazily loaded
        └── history: QueryHistory
```

### 3.2 Surfaces (tabs)

Tabs are heterogeneous — a tab is whatever was last opened.

```rust
enum Surface {
    Query(QueryBuffer),   // editor + result grid
    Table(TableView),     // inner tabs: Data | Structure
}
```

A `Table` surface opens on **Data** — the result of a generated
`SELECT * FROM t LIMIT n` — with **Structure** (columns, datatypes, indexes,
constraints) as a sibling inner tab below the grid.

**Result sets are never their own top-level tab.** They live inside the query
buffer that produced them. Re-running a query replaces its results in place;
there is no result history stack. This is the specific decision that keeps the
tab bar readable after an hour of work.

The tab strip supports drag-to-reorder, close on hover and middle-click, and
edge-fade on overflow scroll.

### 3.3 Sidebar

Shows databases and tables for the active profile, with an inline filter that
**narrows the tree in place** — the hierarchy stays visible and nothing opens.

### 3.4 Command palette and fuzzy open

Two search surfaces with genuinely different jobs, sharing one matcher:

- **`cmd+p`** — flat fuzzy finder over tables and views. Enter opens a `Table`
  surface and the palette dismisses. This is the jump.
- **`cmd+shift+p`** — command palette over verbs.
- **Sidebar filter** — narrows the tree in place. This is the browse.

v1 verbs: switch profile · new query tab · open table · run query · query
history · close tab · toggle sidebar · connection settings.

Both use a single fuzzy scorer (`nucleo-matcher`) with two presentations.
gpui-component ships the list, virtualized scrolling, keyboard navigation and a
searchable input, but **no fuzzy scoring** — that is ours.

---

## 4. Data layer

### 4.1 No driver abstraction

Superseded by `2026-08-26-multi-engine-design.md`, and the prediction in its last
sentence held: the second and third engines were a refactor, not a rewrite,
because the discipline below had been kept. Dispatch is a closed enum in
`src/db/` — still not a trait, for the reason given here.

Postgres only, with no trait boundary. A trait with one implementation is a lie
about the code's generality and it makes every call site harder to read.

The one discipline that costs nothing today: **Postgres-specific types do not
leak into the UI layer.** The grid receives rendered strings and type tags, not
`tokio_postgres::Row`. When a second engine arrives, the boundary is a refactor
rather than a rewrite.

### 4.2 Driver: blocking `postgres`

`postgres = "0.19"` (the synchronous client), called from
`cx.background_executor().spawn(...)`.

This reverses an earlier assumption in favour of `tokio-postgres`. GPUI's
executor is `async-task` over Grand Central Dispatch — it is not tokio, not smol,
and has no reactor. A tokio future scheduled on GPUI's background executor
**panics** the moment it touches a socket or a timer. Bridging is possible (a
`OnceLock<Runtime>` plus a `JoinHandle` handoff, roughly twenty lines) but it is
unnecessary complexity when the blocking client owns its own runtime internally
and the work is already off the main thread.

`sqlx` is rejected on separate grounds: its headline feature is compile-time
query verification, which cannot apply to SQL a user types at runtime. It would
contribute a macro and migration layer that Slate never calls.

### 4.3 Row limits

Every result set is capped. **Default 1000 rows, user-configurable.** The footer
states when a limit is in effect, alongside the fetched byte size.

Rejected alternatives:

- **`LIMIT`/`OFFSET` fetch-on-scroll.** Feels smooth, but each page re-executes
  the query, so rows can shift underneath a scroll. Silent inconsistency is the
  worst property a database client can have.
- **Server-side cursors** (`DECLARE`/`FETCH`). Correct — one snapshot, no drift —
  but holds an idle-in-transaction connection open per result set, which blocks
  vacuum and is an operational hazard against shared databases. This is the
  upgrade path when a fixed limit stops being sufficient.

### 4.4 Cell values

Cells render their **real values**. Geometry and JSONB are not summarized,
projected or rewritten.

Truncation is **visual only** — the grid clips to column width. Clicking a cell
opens a **scrollable value inspector panel** showing the whole value:
pretty-printed for JSONB, WKT for geometry, raw otherwise.

Accepted consequence: full values mean full wire transfer. A thousand rows of
large multipolygon geometry can be a gigabyte. The mitigations are the
configurable row limit and the byte-size readout, which make the cost visible and
adjustable rather than mysterious.

---

## 5. Invariants

These are load-bearing. Violating them is a bug regardless of the benefit.

> **1. Slate never rewrites SQL.** Not queries the user typed, and not queries
> Slate generated. A client that silently alters a statement cannot be trusted
> with the statements that matter.

An earlier draft allowed Slate-generated table previews to project geometry
columns as `ST_GeometryType(g)` plus a point count. It was rejected: users want
the real data, and the resulting rule has no exceptions to remember.

> **2. The result grid is read-only in v1.** No path exists from the grid to a
> mutating statement.

Superseded by `2026-08-23-in-grid-editing-design.md`. The invariant that
replaces it: **no path leads from the grid to a destructive statement**, enforced
by a whitelist gate rather than by the absence of any write path at all.

> **3. No environment-specific behaviour.** No vendor binary names, no
> assumption that a loopback host implies plaintext, no hardcoded ports.

---

## 6. Query execution

`cmd+enter` runs **the selection if there is one, otherwise the statement under
the cursor** — mirroring DBeaver.

Statement boundaries come from the tree-sitter parse tree, which is already in
the build for syntax highlighting. This matters: a naive split on `;` is wrong
for semicolons inside string literals and for `$$`-quoted function bodies, and
hand-writing a correct splitter is real work. The parser makes it free.

A buffer holds many statements. Running one does not run its neighbours.

### Errors

Query errors render **inline below the editor**, never as a modal, with the
Postgres error position mapped back to the offending statement.

Connection errors describe what happened and stop there — *"Connection refused:
nothing is listening on `host:port`"*. They do not speculate about the user's
machine, name third-party binaries, or inspect the process list.

---

## 7. Rendering, theme and motion

### 7.1 gpui from crates.io, exact-pinned

```toml
gpui = "=0.2.2"
gpui-component = { version = "=0.5.1", features = ["tree-sitter-languages"] }
```

Lockfile committed. Verified: this pair resolves 785 packages with zero git
dependencies and passes `cargo check` in about two minutes cold.

**Not forked.** Forking gpui means owning the Metal renderer, text shaping and
the platform dispatcher on a project where the custom interface — not framework
work — is the point. Every upstream fix becomes a manual rebase against a
codebase with no changelog. A well-resourced community fork (`gpui-ce`) already
exists for anyone who needs that.

**Not git-pinned**, for now. Git-pinning is the norm for UI libraries and editors
that need bleeding-edge internals, and it carries real costs: Cargo treats
`git+…/zed` and `git+…/zed?rev=X` as distinct package identities *at the same
commit*, so mixing pin styles silently duplicates gpui; bumping a dependency's
rev drags a different transitive gpui commit; some transitive versions need exact
pins to unify. The two existing GPUI Postgres clients both ship on crates.io.

Accepted cost: a gpui release that has been static for ten months, and no way to
pick up a gpui bugfix without moving to a git pin. Version bumps within
gpui-component are mechanical renames — an afternoon each.

### 7.2 Library machinery, hand-built skin

gpui-component supplies the parts that are invisible when they work:

| Component | Used for |
| --- | --- |
| `InputMode::CodeEditor` | Rope-backed multi-line editor, IME, line numbers, indent guides |
| `src/highlighter/` | tree-sitter highlighting; SQL ships via `tree_sitter_sequel` |
| `src/table/` | Result grid, virtualized on both axes |
| `src/dock/` | Panels and tab bars |
| `Root` dialog layers | Modal overlays for the palette |

**The look is entirely ours.** This is not a compromise, because of how the
library is built: `TableDelegate::render_td(row_ix, col_ix)` is pull-based — the
library owns scroll math and virtualization while **every visible pixel of every
cell is an element Slate writes**. The highlighter likewise emits neutral token
kinds that Slate maps to its own palette. What the library provides is a text
engine and a scroll virtualizer, not an aesthetic.

Hand-rolling those would cost months and be invisible when correct. For
reference, a comparable GPUI application spent 6,417 lines on a single text
input.

### 7.3 Theme — written here, not ported

Slate's theme is its own module, written from scratch. An earlier draft planned to
port `theme.rs` and `motion.rs` from another GPUI project under MIT. That was
rejected: roughly 2,600 lines and a third-party copyright clause in Slate's
license, for an appearance Slate can define itself.

The design principles are standard practice and are what the module implements:

- **Two appearances designed separately, not inverted.** Both follow one
  elevation rule — the closer a surface sits to the data, the more light it
  gets: results brightest, the editor's page one tone behind, chrome furthest
  back. Near-black is absent in dark; it reads as a hole, not a plane. Tone
  steps, not hairlines, separate the planes; borders are reserved for floating
  overlays and the seams that drag handles already own. Accents shift weight
  between appearances at the same hue to hold contrast, and colour is reserved
  for state — controls are neutral greys that brighten under the pointer.
- **An oklch-derived neutral scale**, so lightness steps are perceptually even
  rather than evenly spaced in sRGB.
- **A contrast-ratio function in the theme module itself**, with tests asserting
  text pairs clear WCAG AA in both appearances. Contrast is checked, not assumed.
- **Hairline borders as alpha over content** — a low-alpha wash rather than a
  solid stroke, so edges read soft. Light appearances need proportionally more
  alpha than dark ones to stay visible.
- **A deliberately small scale.** Four spacing values and three radii. Arbitrary
  one-off pixel values are a code-review failure.
- **Numbers drive layout, colours are paint.** Layout constants live separately
  from colour tokens so the two never entangle.

A token-mapping layer bridges gpui-component's own theme tokens to these. Where a
component resists mapping, the escape hatch is hand-rolling that specific widget
while keeping the library's editor and table.

### 7.4 No animation in v1

There is no motion system, no transitions, and no easing curves. Zed is largely
static and reads as fast and modern; "buttery smooth" is a property of frame
timing and input latency, not of things sliding around.

This is a deliberate cut with a real payoff — it avoids the entire class of GPUI
animation hazards in §7.5, including the one that costs 36% CPU. Animation is
deferred (§10) and additive.

The concrete consequence: a query in flight is indicated by static state — text
and a disabled control — not a spinner.

### 7.5 Known GPUI hazards

Not currently reachable, since v1 has no animation. Recorded because they are
expensive to rediscover the day someone adds a loading indicator.

- **A repeating `with_animation` element requests a redraw every display frame
  while mounted.** One spinner has been measured pinning a window at 120Hz and
  36% CPU. The fix is a single shared throttled clock with per-view leases, stale
  leases reaped, parking when the lease list empties.
- **`with_animation` replays from zero on remount**, so anything that must
  survive being unmounted mid-animation needs a wall-clock-driven tween evaluated
  fresh each render.
- **GPUI drops view state the same frame the view unmounts**, so exit animations
  need an explicit open → closing → closed lifecycle with a reaping timer.
- **No scale transform on `div`s** at this revision — SVG only.
- **`.hover()` snaps with no transition.** Colour fades require a manual system.
  In v1 this is a feature: hover states are instant.
- `translateY` is a relative-position inset applied after layout, so siblings do
  not shift.

---

## 8. Storage

| Data | Location |
| --- | --- |
| Profiles (host, port, database, user, display name, colour) | TOML in `~/Library/Application Support/Slate/` |
| Passwords | macOS Keychain |
| Query history | Append-only file per profile |
| Window and pane geometry | Debounce-saved settings file |

The connection form accepts a `postgresql://…` URL and populates its fields,
because pasting a connection string is how connections are actually shared.

**Blank passwords are valid and never warned about**, and usernames containing
`@` must work — both are required by cloud IAM authentication schemes, and both
are rejected by a surprising number of clients.

There is no "test connection" button. Connecting is the test.

---

## 9. Build order

Three phases. The MVP proves the stack works at all; the second phase makes Slate
usable every day; the third makes it usable by someone else.

Each phase is a stopping point where the app runs. No phase leaves the build
broken pending the next one.

### Phase 0 — MVP: connect, query, read results

The goal is the shortest path to a window showing real rows from a real database.
Everything here is foundation that later phases build on, not throwaway.

0. **Confirm the pinned crate pair links and opens a window.** This is the
   riskiest unverified assumption in the spec (§11) and it is cheap to settle.
   `cargo check` has passed; linking Metal shaders and calling
   `Application::new().run()` has not been tried.
1. Window opens. Theme module written, with contrast tests; token-mapping layer
   onto gpui-component's own theme tokens.
2. One hardcoded connection. Blocking `postgres` client on
   `cx.background_executor()`.
3. Query buffer with tree-sitter SQL highlighting. `cmd+enter` running the
   selection or the statement under the cursor. Errors inline below the editor.
4. Result grid via `TableDelegate`. Row limit and byte-size readout.

**MVP is done when** a query typed into the window returns rows into the grid,
and a syntax error shows up under the editor instead of crashing.

### Phase 1 — daily driver

5. Value inspector panel for large JSONB and geometry values.
6. Schema tree in the sidebar, with the inline filter.
7. Profiles, the switcher control, and per-profile isolated state.
8. Surfaces and the tab strip.
9. `cmd+p` fuzzy open and `cmd+shift+p` command palette; `nucleo-matcher`.
10. Table surface with Data and Structure inner tabs.
11. Profile persistence, Keychain, query history.

### Phase 2 — shippable

12. TLS, with an SSL-mode selector. `rustls` versus `native-tls` is deferred
    until the app exists and the decision can be made against real code.
13. Connection form and `postgresql://` string parsing.
14. Homebrew tap with a source-build formula.

### Distribution ladder

1. **Personal Homebrew tap, source-build formula.** No fees, no review. A locally
   compiled binary never receives `com.apple.quarantine`, so Gatekeeper does not
   prompt at all. The cost is a multi-minute compile for the user.
2. **Unsigned `.app` and `.dmg` from CI on tag.** The bundle is free and required
   for macOS system integration; the README covers the Gatekeeper detour.
3. **Signing and notarization.** $99/year, only once demand justifies it.

---

## 10. Deferred, in likely order

Animation and transitions · SSH tunneling · cloud IAM authentication ·
~~CSV export~~ (landed as CSV and JSON — `src/export.rs`; a relation tab exports
what its row-limit chip fetched, so exporting a table larger than the chip's
ceiling waits on the server-side cursors below — a query tab is already uncapped)
· configurable keybindings · views and functions in the schema inspector ·
server-side cursors for unbounded result sets · ~~a second database engine~~
(landed — `2026-08-26-multi-engine-design.md`) ·
~~row editing~~ (no longer deferred — `2026-08-23-in-grid-editing-design.md`).

---

## 11. Unverified

Carried forward as risk, not fact.

- **That the pinned crate pair links and opens a window.** `cargo check` neither
  links nor executes. A ten-minute confirmation, and the first task in Phase 1.
- **Frame rates on a real large result set.** The library ships a million-row
  demo, but that demonstrates the virtualizer, not a cell renderer holding large
  values. Needs measurement against a wide table with geometry.
- **How hard the token-mapping layer fights gpui-component's own theming.** The
  main risk to §7.2 and §7.3.

### Settled

- **The crate name `slate` is taken on crates.io** by an abandoned CLI tool last
  published in 2017. This blocks publishing Slate as a crate and nothing else —
  distribution is a Homebrew tap, whose formulas are namespaced. The app, repo,
  binary and bundle names are all unaffected. A JavaScript rich-text library
  shares the name in a different domain, judged acceptable.

---

## 12. License

MIT. No third-party code is vendored, and Slate's license carries no other
copyright holders — see §7.3 for the decision that keeps it that way.
