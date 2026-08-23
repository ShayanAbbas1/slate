# Slate — In-grid editing

**Date:** 2026-08-23
**Status:** Approved for implementation

Editing a cell in the result grid and having Slate write the `UPDATE`. This
supersedes the read-only-grid position taken in
`2026-08-17-slate-design.md` — see §7.

---

## 1. What this is

The user selects a cell, types a new value, and repeats. Nothing has left the
machine yet. When they apply, Slate generates one `UPDATE` per changed row,
puts the SQL where they can read it, and runs it.

The shape follows the sort path exactly. A header click already causes Slate to
splice an `ORDER BY` into the statement in the buffer and run the result; a cell
edit causes Slate to append an `UPDATE` to the buffer and run it. Both are asks,
both are visible, both leave the statement that ran on screen. This is not a new
category of behaviour, it is the second instance of one that already exists.

**Not in this version:**

| Cut | Why |
| --- | --- |
| `DELETE` from the grid | Forbidden outright by `AGENTS.md` rule 1, whatever the user asks. |
| `INSERT` of new rows | Additive later. Nothing here forecloses it. |
| Editing a primary-key column | See §3. |
| Typing a `NULL` | Needs its own gesture; see §3. |
| Editing a result Slate cannot trace to one table | Refused, loudly. See §2. |

---

## 2. Row identity

**A cell is editable only when Slate can name the row with a primary key.**
Everything else is refused with a note, in the manner of the sort path's *"Slate
cannot add an `ORDER BY` to this statement without rewriting it."*

The provenance comes free. `db::column_types` already prepares the statement to
learn column types, and `tokio_postgres::Column` carries `table_oid()` and
`column_id()` beside the type — Slate was dropping both. Keeping them, plus one
catalog query against `pg_index`, answers the whole question.

A result set is editable when **all** of these hold:

- The type probe succeeded at all. It runs only for a single statement that
  returned columns (`commands == 1`), which is already the condition under which
  Slate knows column types.
- Every column that has a source table has the *same* one. A join is not
  editable.
- That table has a primary key.
- **Every** primary-key column is present in the result set. A `SELECT` that
  omits part of the key cannot identify a row.

Otherwise the result carries no edit target and the grid stays read-only.
Joins, aggregates, views, `SELECT`s that drop the key, and any statement the
server declines to describe all land here.

```rust
pub struct EditTarget {
    pub schema: String,
    pub table: String,
    pub columns: Vec<Option<String>>,  // real column name per result column
    pub keys: Vec<usize>,              // result-column indices forming the key
}
```

`columns` exists because `SELECT id AS ident` must generate SQL against `id`,
not against the alias. `None` marks a computed column, which is displayed and
never edited.

**Hard rule 4 holds unchanged.** No OID and no `column_id` crosses out of
`db.rs`. The oids are plumbing between the probe and the resolver; the UI
receives a resolved answer, not an identifier it would have to interpret.

### Rejected alternatives

- **A full-row `WHERE`** — every visible column in the predicate, no catalog
  work at all. It needs `IS NULL` handling, it produces enormous statements for
  wide or geometry-heavy rows, and when the row is not unique it updates several
  rows without saying so. The user does read the statement first, which is a real
  mitigation, but "silently changed four rows" is the failure this codebase is
  least willing to ship.
- **`ctid`** — cheap and unique, and wrong. It is invalidated by any concurrent
  `UPDATE` or `VACUUM`, and it means Slate writes a predicate the user cannot
  reason about. Technically visible, against rule 1 in spirit.

### Accepted cost

One extra catalog round trip per result set, on a path that has already been to
the network twice. There is no cache. If one is ever needed it goes on
`Connection`, keyed by table — not before it is measured.

The catalog query runs through `internal_query`, so inside a transaction the
user has already aborted it fails. That degrades to "not editable" and must
never turn the user's successful query into an error.

---

## 3. Editing

Pending edits live in the `ResultGrid` delegate, beside `result` and `display`.
`result.rows` is never written — it stays as fetched, so the grid can always
show what has changed against what the server holds, and discarding is
clearing a `Vec`.

Clicking a cell makes it active; `Enter` opens an input on it, `Enter` commits to
the pending set, `Esc` cancels. Double click opens the editor too, on any cell
that has one to open — every other data grid treats double click as "act on
this cell now", and there is no longer a reason for Slate not to. The gesture it
replaces, double-click-to-copy, predates editing and lost the argument for being
its own gesture: it fired on cells that could not be edited and cells that
could, which meant the same double click did two different things depending on
provenance the user cannot see. The `copied` field that tracked it is gone with
it; copying moved to `cmd+c`, below.

**The active cell is Slate's, not the library's.** gpui-component tracks a
selected row *or* a selected column as mutually exclusive modes and never a
cell, so `selected_row` and `selected_col` are not two halves of a cell
coordinate — reading them as though they were is what made the first
implementation of this section silently do nothing. The active cell lives in the
delegate as its own per-cell coordinate, and it is painted with `accent` so a
keystroke never acts on an invisible target.

**But there is exactly one of it.** The library's arrow-key actions move its own
selection, and a ring that does not follow them means the user navigates to one
cell and `Enter` edits another — two notions of position, of which the user's
last action is the one that has to be respected. Rather than bind arrow keys
against the library's own actions, Slate subscribes to what those actions emit
and folds it in: `SelectRow(r)` makes the active cell `(r, current column or
column 0)`, `SelectColumn(c)` makes it `(current row or row 0, c)`. Moving down
a column is not moving out of it, so the other half is kept; with nothing active
yet the missing half is an origin, because a keystroke on a grid has to leave the
ring somewhere readable.

Every route to the active cell therefore ends in `set_active`, and the two that
fire together converge. A cell click sets the coordinate whole *and* makes the
library select the row, so the fold arrives with the same row and the column the
click just recorded — the ring lands on the clicked cell whichever of the two
runs first, which is what keeps this from being the same bug wearing a
subscription. The fold is hooked where the grid entity is constructed, not at a
call site: the query tab and every relation tab build theirs through the same
function, and a subscription attached to one of them would leave the others
navigating an invisible selection.

A **header click** is not special-cased, deliberately. It reaches the fold as a
column change, and on a sortable result that is unobservable — the sort re-runs
the statement and replaces the delegate wholesale, so the ring is dropped a
frame later either way. On a result that cannot be sorted, the click is refused
with a note and the ring sits at the top of the column the user just pointed at,
which is where their last action was. Telling the two clicks apart would need
state whose only job is to disagree with the rule above.

The ring moving is also how an open input closes. `set_active` drops an editor on
any other cell, so an arrow key reaches that guard by the same route a click
does; an input holding focus on one cell while the ring sits on another is two
cells claiming the keyboard and `Enter` acting on neither.

**`cmd+c` copies the active cell**, in the `Table` key context beside `enter`.
This is not a convenience: §2 of `2026-08-17-slate-design.md` ships no CSV or
Parquet export on the grounds that *"Clipboard copy covers the common case"*, so
a build with no way to copy a value has quietly withdrawn a decision that was
already argued. It reads the fetched row, not the rendered string, for the same
reason the row inspector does — a column is a couple of hundred pixels wide and a
JSONB document is not, and copying what happened to fit would be a truncation
nobody asked for. It is also the only copy gesture that works on the cells this
whole document refuses to edit: a join, an aggregate, a view, a primary-key
column. Those can never open an input, so "select it in the editor and copy"
covers none of them.

It does nothing while an input is open. There `cmd+c` is the input's own text
selection, and a grid copy firing over it would replace what the user just
selected with the whole cell. The gate is the delegate's: with an editor open it
offers no value at all, so the keystroke has nothing to write and the input's
deeper binding is the only one that acts. There is no per-cell "copied" mark —
`cmd+c` gives no feedback anywhere else on the platform either, and the tint
that used to exist was per-cell state whose whole job was to say a keystroke
happened.

Only the one cell being edited renders an input. Cells with pending edits get a
tone; every other cell stays on the path that `render_td`'s
must-not-allocate constraint is about.

**Primary-key columns are not editable.** `SET id = new WHERE id = old` is
legal SQL and occasionally what someone wants, but it is the one edit whose
result cannot be re-verified afterwards — the row it identified no longer
answers to the key that found it. The refusal says so.

**There is no way to type a `NULL`.** An emptied cell is an empty string.
Distinguishing the two needs a gesture of its own, and that decision is better
made after the feature has been used than before. This is a known gap, not an
oversight.

---

## 4. Applying

One generator, two destinations, fanning out on the active tab the way
`sort_column` already does.

One `UPDATE` per changed row, with every changed column in a single `SET`. The
statements are joined into one string, so one `simple_query` round trip is one
implicit transaction and the batch is all-or-nothing without any transaction
code — which matters, because `db.rs` has no transaction API and the type probe
already has a documented hole around transactions the user opened.

- **Query tab** — append the statements to the buffer, run them, then re-run the
  `SELECT` that produced the grid.
- **Table tab** — a modal showing the statements with a Run button, then the
  existing relation refresh.

The modal is not an exception to rule 1. The rule requires that the statement
which runs is the statement on screen; a table tab has no buffer to put it in,
so the modal is the screen. Run is the ask, and nothing runs before it.

Two details that fall out of the existing code rather than out of this design:

- `execute_sql` refuses to start while a query is running, so the refresh has to
  be chained inside the completion of the batch, not called after it.
- The query tab keeps no record of the statement behind the current grid, and
  after appending an `UPDATE` the cursor no longer sits on the `SELECT`. The
  originating statement is stashed when a result lands. This is a deliberate
  exception to the sort path's rule of deriving state from the buffer text
  instead of holding it — there, the text still says what happened; here, Slate
  has changed the text underneath.

Apply reports no row count. `rows_affected` is unreliable over the simple
protocol, as `db.rs` documents, and a wrong count is worse than none.

### An edited row moves, and that is left alone

After an apply, the refreshed grid usually shows the edited row somewhere else —
typically last. This is not a sort and not a defect in the refresh. Postgres does
not update a row in place: under MVCC an `UPDATE` writes a new tuple version,
generally at the end of the heap, and marks the old one dead. A `SELECT` with no
`ORDER BY` has no defined order, so a sequential scan returns roughly physical
order and the new version comes back last.

**Deliberately not fixed.** Two things were considered and both rejected:

- Adding an `ORDER BY` to the user's own `SELECT` in a query tab. Forbidden by
  `AGENTS.md` rule 1 — a header click is how the user asks for that, and it
  splices visibly.
- Ordering Slate's generated table preview by primary key. Tempting, because an
  unordered `LIMIT` returns an arbitrary *subset* rather than merely an arbitrary
  order, which is the silent inconsistency the 2026-08-17 spec §4.3 rejects
  fetch-on-scroll for. Declined anyway: it imposes a sort on every table tab,
  including tables large enough that a sequential scan stopping at the limit is
  much cheaper than an ordered read, and slowing every preview to stabilise the
  rare edited row is the wrong trade.

The capability is already available per tab and opt-in: a header click adds an
`ORDER BY` to a table tab's generated statement, so anyone who wants stable row
positions can have them and pay for them. Do not make that the default without
raising it.

---

## 5. The guard

Every generated statement passes one gate before anything runs:

```rust
pub fn is_generated_update(sql: &str) -> bool
```

It is a whitelist. The statement must parse whole, every root statement must be
an `update`, and no `delete`, `drop` or `truncate` node may appear anywhere in
the tree — `WITH x AS (DELETE FROM t RETURNING *) UPDATE …` is a real statement
shape and it does not pass.

This is the code half of a rule that until now existed only in prose.
`AGENTS.md` rule 1 forbids Slate from ever writing a destructive statement; a
whitelist makes that structural rather than a list of names to remember.

An empty key set cannot produce a statement. An `UPDATE` with no `WHERE` changes
every row in the table, so the generator refuses rather than emitting one.

---

## 6. Testing

`sql.rs` generation and the gate are pure string-in/string-out and tested that
way, in the manner of the existing splice tests: statement shape, composite keys,
apostrophes in values, quotes in identifiers, each destructive form refused, a
mostly-`UPDATE` batch with one `DELETE` refused, and the generator's own output
accepted by the gate so the two cannot drift apart.

Identity resolution is factored so the "one table, all keys present" decision is
pure and testable without a server. The rest is `live_`-prefixed: a single-table
select is editable, a join is not, an aggregate is not, a keyless table is not, a
select missing part of the key is not, an alias resolves to its real column, a
composite key yields both indices.

`live_the_type_probe_leaves_an_open_transaction_alone` must keep passing
unchanged. It is the regression test for the property this change is most likely
to break.

---

## 7. What this supersedes

- **§2 of `2026-08-17-slate-design.md`**, whose non-goals table lists *"Row
  editing / `UPDATE` via grid — Slate is an editor, not a data-entry surface. The
  grid is read-only."* Superseded on that point only. The reasoning still holds
  for what is cut in §1 above: this is not a data-entry surface, it is an editor
  that can write one statement kind when asked.
- **§5 invariant 2 of the same document** — *"The result grid is read-only in v1.
  No path exists from the grid to a mutating statement."* Superseded. The
  invariant that replaces it: **no path leads from the grid to a destructive
  statement**, enforced by §5 rather than by the absence of any path at all.
- **§10** lists row editing last among deferred work. It is no longer deferred.

`AGENTS.md` rule 2 is amended to match. Rule 1 already anticipated this change
and needs none.
