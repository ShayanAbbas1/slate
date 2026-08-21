# AGENTS.md

Slate is a native macOS Postgres client in Rust on GPUI. A SQL editor that shows
results — not a database browser with an editor bolted on.

**Read `docs/specs/2026-08-17-slate-design.md` before doing anything.** It carries
the reasoning behind every decision below, including the rejected alternatives.
This file is the operational summary; the spec is the source of truth.

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

2. **The result grid is read-only until row editing exists.** No code path leads
   from the grid to a mutating statement, and none may lead to a destructive
   one afterwards either.
3. **No environment-specific behaviour.** No vendor binary names in error
   strings, no assumption that a loopback host means plaintext, no hardcoded
   ports or hostnames. Slate is a generic client.
4. **Postgres types do not reach the UI layer.** The grid receives rendered
   strings and type tags, never driver row types. This is the only concession to
   a future second engine — do not add a driver trait.
5. **Blank passwords are valid.** Never warn about them. Usernames containing `@`
   must work. Both are required by cloud IAM auth and both are commonly broken.
6. **Errors describe what happened, not what to do about it.** "Connection
   refused: nothing is listening on `host:port`" and stop. No speculation about
   the user's machine, no process-list inspection.

---

## Stack

```toml
gpui = "=0.2.2"
gpui-component = { version = "=0.5.1", features = ["tree-sitter-languages"] }
postgres = "0.19"          # blocking client, NOT tokio-postgres
nucleo-matcher = "*"       # fuzzy scoring; gpui-component ships no scorer
icondata_lu = "=0.1.0"     # Lucide icon data; gpui-component ships no icon files
```

**Pins are exact and the lockfile is committed. Do not bump without being asked.**
gpui is pre-1.0 and breaks on minor bumps; `main` has declared `0.2.2` for ten
months, which is a stalled version field rather than parity with the release.

**Do not add tokio.** GPUI's executor is `async-task` over Grand Central
Dispatch. A tokio future on `cx.background_executor().spawn(...)` _panics_ the
moment it touches a socket or timer. Database work uses the blocking `postgres`
client, which owns its runtime internally, spawned onto the background executor.

**Do not fork gpui.** Decided in the spec, §7.1.

### Local build and run

```sh
cargo build
docker compose up -d
cargo run
```

The app opens the connection form when no `PG*` environment is configured. The
repository-owned development database accepts:

```text
postgresql://slate:slate@127.0.0.1:55432/slate_dev
```

Paste that URL into the form and choose **Use URL**, then **Connect**. Connecting
is the connection test; there is deliberately no separate test button.

### What gpui-component provides

Use these rather than hand-rolling: `InputMode::CodeEditor` (rope-backed
multi-line editor, IME, line numbers), `src/highlighter/` (tree-sitter; SQL via
`tree_sitter_sequel`), `src/table/` (grid virtualized on both axes),
`src/dock/` (panels, tab bars), `Root` dialog layers (modal overlays).

It does **not** provide a fuzzy matcher or a command palette. Those are ours.

It also does **not** ship the icons its `IconName` names: those are Lucide file
paths with no files behind them. `src/icons.rs` is Slate's `AssetSource` — it
serves the same paths from `icondata_lu` in memory, so nothing is vendored into
the repository and the library's own widgets get their icons from it too. Add a
row to `ICONS` when something needs one; an unlisted path draws nothing.

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
