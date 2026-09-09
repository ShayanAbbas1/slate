# AGENTS.md

Slate is a native macOS SQL client in Rust on GPUI, speaking Postgres, MySQL
and SQLite. A SQL editor that shows results — not a database browser with an editor
bolted on.

**Read this file before doing anything.** It is the source of truth for how
Slate is built and why.

The design documents behind it live in `docs/specs/` and are deliberately not
version-controlled — the reasoning and the rejected alternatives are working
notes, not something to publish. Read them if they are on your disk; the
`spec §` references in the source comments point into them. A clone will not
have them, and nothing in the repository should come to depend on them.

---

## Hard rules

Violating one of these is a bug regardless of the benefit. If a task seems to
require it, stop and raise it instead.

1. **Never rewrite SQL behind the user's back.** No silent `LIMIT` injection, no
   column projection, no reformatting on execute, and nothing at all on a
   statement the user did not ask Slate to change. Row limits apply to
   Slate-generated preview queries only, and they are visible in the UI.

   Slate _does_ write SQL when the user asks it to, and only then. A header
   click asking for a sort is such an ask: the `ORDER BY` is spliced into the
   statement in the buffer, where the user can read it, edit it and undo it,
   and the statement that runs is the statement on screen. The same will hold
   for in-place row editing.

   Two limits on what Slate may write. It never writes a **destructive**
   statement — no `DROP`, no `TRUNCATE`, no `DELETE` — whatever the user asked
   for. And it never writes into a statement it cannot parse whole:
   `sql::with_order_by` refuses rather than guessing at a clause boundary,
   because a corrupted statement is worse than an unsorted grid.

   This replaces the earlier absolute rule, and supersedes invariant 1 of the
   spec's §5 on this point. The reasoning there — that a client which silently
   alters statements cannot be trusted with the statements that matter — is
   why "silently" is still the word that carries the rule.

2. **No code path leads from the grid to a destructive statement.** The grid can
   now write an `UPDATE`, and `sql::is_generated_update` is the single gate
   every generated statement
   passes first. It is a whitelist, so `DROP`, `TRUNCATE` and `DELETE` are
   refused structurally rather than by name. Do not add a second path that
   bypasses it.

   A cell is editable only when Slate can name its row by primary key. When it
   cannot, the grid stays read-only and says why; it never guesses at a
   predicate.
3. **No environment-specific behaviour.** No vendor binary names in error
   strings, no assumption that a loopback host means plaintext, no hardcoded
   ports or hostnames. Slate is a generic client.
4. **Driver types do not reach the UI layer.** The grid receives rendered
   strings and type tags, never a `postgres::Row`, a `mysql::Value`, a
   `rusqlite::ValueRef`, an OID or a storage class. Engine dispatch is a closed enum inside `src/db/`
   and stops there: no trait, no plugin surface, and no code above `src/db/`
   that branches on which engine is connected.

   This replaces the earlier wording, which said the rendered-string rule was
   "the only concession to a future second engine — do not add a driver trait".
   That was written when the second engine was hypothetical. The reasoning is
   unchanged, and is why the rule survives at all: a UI that knows which engine
   it is talking to grows an engine-shaped special case in every view, and
   those are the special cases nobody ever removes. An enum rather than a trait
   for the same reason in miniature — three arms the compiler makes every match
   enumerate, instead of an open extension point.

   The one thing that legitimately crosses out is `db::Engine`, and only
   because Slate writes SQL: `explorer::preview_sql` and `sql::update_row` have
   to quote an identifier the way the server will read it. It answers three
   questions and holds no connection.
5. **Blank passwords are valid.** Never warn about them. Usernames containing `@`
   must work. Both are required by cloud IAM auth and both are commonly broken.
6. **Errors describe what happened, not what to do about it.** "Connection
   refused: nothing is listening on `host:port`" and stop. No speculation about
   the user's machine, no process-list inspection.
7. **An `sslmode` is never quietly weakened.** A connection either gets what it
   asked for or fails saying which certificate check failed. This is a rule
   because the failure is silent by construction: the driver's default is
   `prefer`, and `prefer` with a connector that cannot do TLS hands back a
   plaintext socket without even sending an SSLRequest — a cleartext password
   under a UI reporting success. That was the bug for as long as `sslmode` was
   dropped on the way in. If a mode cannot be honoured, refuse it by name;
   `tls::SslMode::parse` does that for `allow`, which libpq defines in an order
   the driver cannot express.

---

## Stack

```toml
gpui = "=0.2.2"
gpui-component = { version = "=0.5.1", features = ["tree-sitter-languages"] }
postgres = "0.19"          # blocking client, NOT tokio-postgres
rusqlite = "0.40"          # bundled + column_metadata + column_decltype
mysql = "28"               # rust-mysql-simple, blocking; rustls-tls-ring NOT rustls-tls
nucleo-matcher = "=0.3.1"  # fuzzy scoring; gpui-component ships no scorer
icondata_lu = "=0.1.0"     # Lucide icon data; gpui-component ships no icon files
rustls = "0.23"            # TLS; the driver ships none. default-features = false
tokio-postgres-rustls = "0.14"
```

**`default-features = false` on `rustls` is load-bearing.** Its defaults select
the `aws-lc-rs` provider; `ring` is what is already linked through gpui. Every
crate in the graph has to agree on one or both get built, and `aws-lc-rs` builds
C and assembly. `rustls`, `rustls-native-certs` and `rustls-pemfile` were all
already transitive dependencies, which is why TLS is `rustls` and not
`native-tls` — see the module header in `src/tls.rs` for the rest of that
reasoning.

**Pins are exact and the lockfile is committed. Do not bump without being asked.**
gpui is pre-1.0 and breaks on minor bumps; `main` has declared `0.2.2` for ten
months, which is a stalled version field rather than parity with the release.

**`default-features = false` on `rusqlite` is load-bearing too.** 0.40's
defaults pull in `ffi-sqlite-wasm-rs`, a WASM backend with no business in a
native build. `bundled` compiles the amalgamation rather than linking whatever
libsqlite3 macOS shipped; `column_metadata` is what makes in-grid editing
reachable, since it is the only way to learn that a result column is
`accounts.id` and not an expression.

**Do not add tokio.** GPUI's executor is `async-task` over Grand Central
Dispatch. A tokio future on `cx.background_executor().spawn(...)` _panics_ the
moment it touches a socket or timer. Database work uses blocking drivers, which
own their runtimes internally, spawned onto the background executor. `rusqlite`
is blocking by construction and has no runtime at all.

`tokio-rustls` in the tree is not a breach of that rule, and the rule is why:
the TLS handshake is a future belonging to the connection, so it runs inside the
runtime the blocking client already owns, on the same thread as the connect it
is part of. Nothing tokio-shaped reaches GPUI's executor. Adding a tokio future
anywhere Slate spawns one still panics.

**Do not fork gpui.** Decided in the spec, §7.1.

### Local build and run

```sh
cargo build
docker compose up -d
cargo run
```

The app opens the connection form when no `PG*` environment is configured. The
repository-owned development databases accept:

```text
postgresql://slate:slate@127.0.0.1:55432/slate_dev
mysql://slate:slate@127.0.0.1:53306/slate_dev
```

Pick the engine on the form's chip row first — it decides which fields exist.
Then paste a URL and choose **Use URL**, or fill the fields in. Connecting is
the connection test; there is deliberately no separate test button.

SQLite has no server to connect to. Build the file once, then give the form its
absolute path:

```sh
sqlite3 dev/slate_dev.db < dev/sqlite/001-slate-demo.sql
```

**The MySQL container reports itself healthy when its init script failed.**
`mysqladmin ping` does not care whether the seed applied, so a half-seeded
database looks exactly like a good one. Check a row count, not the status —
`live_the_development_database_is_fully_seeded` is that check.

### Engine divergences

Decided, recorded in the multi-engine spec, and not to be re-litigated:

- **`Engine` is the only engine-shaped thing above `src/db/`**, and only because
  Slate writes SQL. It answers three questions — quote an identifier, quote a
  literal, qualify a name — plus the inverse used to read a sort key back.
  There are **four** call sites that generate SQL, not three:
  `explorer::preview_sql`, `sql::with_order_by`, `sql::update_row`, and
  `main::sort_expression`. The last one is the one that gets forgotten, and
  forgetting it is silent: a double-quoted name is a *string literal* in MySQL,
  so `ORDER BY "name"` sorts every row by the same constant with no error.
- **MySQL and SQLite both bracket a generated multi-row batch** in
  `BEGIN`/`COMMIT`, because each commits every statement on its own where a
  Postgres `simple_query` submission is one implicit transaction. The brackets
  go in the statement text, never around it invisibly, and
  `sql::is_generated_update` refuses a transaction it cannot see closed.
  `Engine::transaction_start` answers which engine needs one, with an arm per
  engine; it was a `_ =>` catch-all at `main::update_batch` until 2026-09-08,
  which is how MySQL went unbracketed while this file claimed it was atomic.
  `BEGIN` rather than MySQL's own `START TRANSACTION` because the gate has to
  read the brackets back and the tree-sitter grammar knows only the first —
  MySQL takes it as an alias outside a stored program.
- **A batch that fails part way is rolled back, and the error says which state
  the data is in.** Without that the brackets produce a third state — neither
  applied nor discarded, and rendered as applied, because the refresh `SELECT`
  runs on the same long-lived connection and reads the uncommitted rows back.
  Only a transaction *this* submission opened is rolled back; one the user began
  in an earlier run is theirs to finish. SQLite asks `is_autocommit` before and
  after; MySQL cannot, because the driver keeps the server's
  `SERVER_STATUS_IN_TRANS` flag private, so it reads the submitted text instead.
- **A statement timeout is one number per profile, applied at connect**, and
  each engine buys something different with it. Postgres's `statement_timeout`
  bounds any statement; MySQL's `max_execution_time` bounds read-only `SELECT`s
  only, so a runaway `UPDATE` or `ALTER` there is Cancel's problem alone, and a
  server older than 5.7.8 (or MariaDB, which spells it differently) fails the
  connect rather than the statement; SQLite has no such setting and gets a
  wall-clock timer firing `sqlite3_interrupt`, which counts waiting on a lock
  the same as scanning. It goes in at connect and never into the user's
  submission — hard rule 1, and on Postgres a `SET` inside their submission
  would be scoped to the implicit transaction around it. It therefore bounds
  Slate's own catalog and structure queries too, which is intended.
- **Cancel reaches the running statement and nothing queued behind it.** The
  handle it needs — Postgres's `CancelToken`, MySQL's connection id, SQLite's
  `InterruptHandle` — is captured in each engine's `open`, before the client
  goes behind the connection mutex, because the statement being cancelled is
  holding that mutex. `Connection::cancel` takes `&self` and locks nothing.
- **`CHECK` constraints are absent** from the Structure tab on MySQL and SQLite.
  SQLite keeps them only in the `CREATE TABLE` text; MySQL's
  `information_schema.CHECK_CONSTRAINTS` only exists from 8.0.16.
- **MySQL verifies certificates against `webpki-roots`**, not the Keychain the
  Postgres path reads. It fails loudly, which rule 7 permits.
- **Geometry is Postgres-only.** MySQL has a `GEOMETRY` type; rendering it is a
  separate decision nobody has asked for.

### What gpui-component provides

Use these rather than hand-rolling: `InputMode::CodeEditor` (rope-backed
multi-line editor, IME, line numbers), `src/highlighter/` (tree-sitter; SQL via
`tree_sitter_sequel`), `src/table/` (grid virtualized on both axes),
`src/dock/` (panels, tab bars), `Root` dialog layers (modal overlays), and
`src/input/lsp/` plus `src/input/popovers/` (the completion provider trait and
the caret-anchored popup it drives).

**It does ship completion infrastructure, and this file said otherwise until
2026-09-09.** `input/lsp/completions.rs` defines `CompletionProvider`, two
required methods, and `InputState::lsp.completion_provider` is a public field.
No language server is involved -- `lsp_types` is the vocabulary and nothing
starts a process. `Render for InputState` draws the menu itself, so a provider
is the whole integration: nothing to render, nothing to anchor, and `up`,
`down`, `enter` and `escape` are already routed to the menu when it is open and
to the cursor when it is not.

Do not hand-roll a popup beside it. The caret's pixel position it would need --
`LastLayout::cursor_bounds`, `InputState::last_layout`,
`line_and_position_for_offset` -- is `pub(super)` with no accessor, so an
anchored overlay of our own is fork-only, and forking is out (spec §7.1).

One consequence worth knowing before writing an `escape` handler: the library
binds `escape` scoped to `Input`, Slate binds it unscoped, and an unscoped
binding ties at every depth and wins on registration order. `show_editor` must
therefore `cx.propagate()` on the paths where Slate has nothing stacked to
close, or the completion popup cannot be dismissed.

It does **not** provide a fuzzy matcher or a command palette. Those are ours —
`src/palette.rs` scores with `nucleo-matcher` and presents through the
library's `ListState`, which owns the search field, the virtualized scroll and
the click-to-confirm. A palette row carries a `Command`; `Workspace::run_command`
routes every one of them into the method its button or keystroke already calls,
so the palette is never a second implementation of anything.

It also does **not** ship the icons its `IconName` names: those are Lucide file
paths with no files behind them. `src/icons.rs` is Slate's `AssetSource` — it
serves the same paths from `icondata_lu` in memory, so nothing is vendored into
the repository and the library's own widgets get their icons from it too. Add a
row to `ICONS` when something needs one; an unlisted path draws nothing.

Its `Button` is worth using for the mechanism and nothing else. **Never
construct one directly** — go through `button`, `icon_button` and `button_label`
in `main.rs`, which measure the box off `CONTROL_HEIGHT*` and put the label and
the icon in as children carrying their own colour. That last part is not style:
0.5.1 tints button content `red_400` on hover from a hardcoded colour, and a
child that sets a colour is the only thing that colour does not reach. GPUI's
`.hover()` panics if called twice, so there is no fixing it from outside.

The library owns scroll math, text shaping and virtualization. **Every visible
pixel is still ours** — `TableDelegate::render_td(row_ix, col_ix)` is pull-based,
and the highlighter emits neutral token kinds that we map to our own palette.
Never accept a library default appearance; map it to our theme tokens.

---

## GPUI hazards

Hard-won and easy to rediscover. Read before writing any animated element.

- **A repeating `with_animation` element requests a redraw every display frame
  while mounted.** One spinner has been measured pinning a window at 120Hz and
  36% CPU. Any query-in-flight indicator must use a single shared throttled clock
  with per-view leases, reaping stale leases and parking when the list empties.
- **`with_animation` replays from zero on remount.** Anything that must survive
  being unmounted mid-animation needs a wall-clock-driven tween evaluated fresh
  each render, not an element-id-keyed animation.
- **GPUI drops view state the same frame the view unmounts.** Exit animations
  need an explicit open → closing → closed lifecycle plus a reaping timer.
  Every dropdown, toast and modal hits this.
- **No scale transform on `div`.** SVG only at this revision. Approximate with
  fade plus translate.
- **`translateY` is a relative-position inset** applied after layout, so siblings
  do not shift.
- **`.hover()` snaps with no transition.** Colour fades are manual — see the
  ported `motion.rs` hover-fade system.

Two more that are not about animation, and cost a round each to find:

- **A window with nothing focused has no dispatch path, so every keybinding is
  dead.** A keystroke reaches a handler only along the focused element's path to
  the root. Unmount whatever had focus — close a modal, switch to a surface with
  no focusable element — and the app stops responding to the keyboard entirely
  until something is clicked. Anything that takes focus away must hand it back;
  `Workspace::close_palette` is the worked example, and `Focus::Window` is the
  floor under it for surfaces that have nothing to type into.
- **A binding with no context predicate wins over a scoped one.**
  `Keymap::binding_enabled` scores an unscoped binding at `contexts.len()` —
  the maximum — while `Some("Foo")` scores at the depth of that node, and the
  deepest match takes the keystroke. So a library binding scoped to an inner
  element beats the container's, which is why the palette's arrows are bound
  against `Palette > Input`: a descendant predicate matches at the leaf, which
  is the only depth that takes them back from gpui-component's input. Ties are
  broken by registration order, and Slate's `cx.bind_keys` runs after
  `gpui_component::init`.

---

## Conventions

- **Comments explain why, never what.** Code should carry its own meaning. Add a
  comment when the code cannot convey a decision that is non-obvious from
  reading it.
- **No speculative abstraction.** No trait with one implementation, no factory
  for one product, no config for a value that never changes. The spec's Non-goals
  section is binding — do not build ahead of it.
- **Deletion over addition.** The shortest change that fully solves the problem
  wins, once the problem is actually understood.
- A `ponytail:` comment marks a deliberate simplification and names its ceiling
  and upgrade path. Leave one where a shortcut is a decision, not an oversight.
- Non-trivial logic leaves one runnable check behind — the smallest test that
  fails if the logic breaks. No fixture scaffolding.
- Match surrounding code's naming, density and idiom.

---

---

## No animation in v1

There is no motion system, no transitions, no easing curves. Hover states are
instant. A query in flight is shown with static text and a disabled control, not
a spinner.

This is deliberate — see the spec §7.4. It also means none of the GPUI animation
hazards above are currently reachable. Do not introduce `with_animation` without
raising it first.
