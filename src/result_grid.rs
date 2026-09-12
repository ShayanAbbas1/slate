use gpui::{
    App, AppContext, Context, Entity, Focusable, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    InteractiveElementExt,
    input::{Input, InputState},
    table::{Column, TableDelegate, TableState},
};

use crate::{
    db::{self, EditTarget, QueryResult},
    icons::icon,
    store::{GRID_ROW_CAP, StoredGrid, captured_at},
    theme::{layout, theme},
};

/// ponytail: a column is a few hundred pixels wide, so shaping more than this is
/// work nobody can see -- and `db` deliberately keeps whole values, which run to
/// megabytes for JSONB and PostGIS. The row inspector is where a whole value
/// gets read; this is the visual-only clip the spec's §4.4 allows.
const CELL_DISPLAY_LIMIT: usize = 300;

/// ponytail: the inspector shows the value, not a column's worth of it -- but
/// "the value" has to stop somewhere, because a multi-megabyte document laid
/// out as wrapped text stalls the frame it is laid out in. `cmd+c` on the cell
/// still copies all of it. Raise this if a real value gets cut.
const FIELD_DISPLAY_LIMIT: usize = 4_000;

/// What an absent value is called wherever one is shown.
pub const NULL_LABEL: SharedString = SharedString::new_static("NULL");

/// The advance width of one character in the grid's monospaced face.
///
/// ponytail: a character count times one advance, not real text measurement --
/// `TableDelegate` is asked for a column width long before there is a text
/// system to measure with. It is exact for ASCII, which is what column names
/// and most values are, and a column of wide glyphs is one drag from right.
const CHAR_WIDTH: f32 = 7.8;

/// Room for the sort control that rides at the trailing edge of a header, so a
/// column sized to its own name still shows the whole name.
const HEADER_CONTROL_WIDTH: f32 = 20.0;

const MIN_COLUMN_WIDTH: f32 = 56.0;
const MAX_COLUMN_WIDTH: f32 = 480.0;

/// ponytail: the widest of the first rows, not of every row. A column width is
/// a first impression and a drag corrects a wrong one, so measuring 5,000 rows
/// of geometry to place a column is time the user waits through for nothing.
const WIDTH_SAMPLE_ROWS: usize = 200;

/// A row's schema and table, and its primary key as column/value pairs — what a
/// one-row `DELETE` needs and nothing else.
pub type RowKey = (String, String, Vec<(String, String)>);

pub struct ResultGrid {
    columns: Vec<Column>,
    result: QueryResult,
    /// Clipped, ref-counted copies built once per result set. `render_td` runs
    /// for every visible cell on every frame, so it must not allocate.
    display: Vec<Vec<Option<SharedString>>>,
    /// The `ORDER BY` the rows arrived in, as column indices: the server did
    /// the sorting, so this is a readout of the statement that ran, not a state
    /// the grid can change on its own.
    sort: Vec<(usize, bool)>,
    /// Whether a header click can sort this result at all. A control that does
    /// nothing is worse than no control.
    sortable: bool,
    /// The cell a keystroke acts on. Slate's, not the library's: gpui-component
    /// tracks a selected row *or* a selected column as mutually exclusive
    /// modes and never a cell, so a coordinate has to be assembled here or
    /// `Enter` has no target. A click sets it outright; the library's arrow
    /// keys reach it through `select_row` and `select_col`. Both paths end in
    /// `set_active`, so there is one answer to where the user is.
    active: Option<(usize, usize)>,
    /// What the user has changed and not yet applied. `result.rows` is never
    /// written, so the grid can always show pending against as-fetched and
    /// discarding is dropping this.
    ///
    /// ponytail: a linear scan per visible cell per frame, over the handful of
    /// cells one person edits between applies. A map keyed by `(row, col)` is
    /// the upgrade path if that handful ever becomes thousands.
    pending: Vec<PendingEdit>,
    /// The one cell showing an input, if any. At most one: every other cell
    /// stays on the fast path that `display`'s no-allocation rule is about.
    editing: Option<Editing>,
    /// When these rows were snapshotted, for a grid that came off disk.
    ///
    /// Held here rather than on the tab because a completed run replaces the
    /// whole delegate: there is no field anyone has to remember to clear, and
    /// so no way for a live result to keep claiming it is a snapshot.
    captured: Option<u64>,
    /// How many rows the result had, for a snapshot that was capped before it
    /// was written. Held beside `captured` and for the same reason: a run
    /// replaces the whole delegate, so a live result cannot keep a stale count.
    restored_total: Option<usize>,
    /// Which result columns carry a foreign key, as indices into `columns`.
    /// Empty until a relation's structure says otherwise, and empty forever on
    /// a query result: a statement can join as many relations as it likes, so
    /// there is no one relation whose keys these columns could be.
    foreign_keys: Vec<usize>,
    /// The hover group each key column's cells share, one per key column and
    /// built where the keys are marked. `render_td` runs for every visible cell
    /// every frame under a no-allocation rule, and a group named there would be
    /// a `format!` per cell per frame.
    follow_groups: Vec<SharedString>,
}

/// One changed cell, held beside the fetched value rather than over it.
struct PendingEdit {
    row: usize,
    col: usize,
    /// What will be written. Whole, because this is what the `UPDATE` carries.
    /// `None` is a `NULL`, which is a different write from the empty string.
    value: Option<SharedString>,
    /// What the column paints, clipped for the same reason `display` is, and
    /// absent for the same reason a fetched NULL's display cell is: the cell
    /// already paints an absence in italics, and a second spelling of `NULL`
    /// on screen is one too many.
    shown: Option<SharedString>,
}

struct Editing {
    row: usize,
    col: usize,
    /// Built on the first render of the cell, because an input needs a window
    /// and opening an edit deliberately does not.
    input: Option<Entity<InputState>>,
}

/// One row's worth of pending edits, resolved to real column names and ready
/// for `sql::update_row`. Alias resolution happens here so the caller does none.
pub struct PendingRow {
    pub schema: String,
    pub table: String,
    /// Real column name and its new value, one per changed column. `None` is a
    /// `NULL`.
    pub sets: Vec<(String, Option<String>)>,
    /// The key columns' real names against their **as-fetched** values: the row
    /// is identified by what the server holds, not by what the user has typed.
    pub keys: Vec<(String, String)>,
}

impl ResultGrid {
    pub fn empty() -> Self {
        Self::new(QueryResult::default())
    }

    pub fn new(result: QueryResult) -> Self {
        let display: Vec<Vec<Option<SharedString>>> = result
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.as_deref().map(|value| clip(value).into()))
                    .collect()
            })
            .collect();
        let columns = result
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                Column::new(index.to_string(), column.name.clone())
                    .width(fitted_width(&column.name, &display, index))
                    .resizable(true)
                    .movable(false)
                    // The cell padding is Slate's, applied in `render_td` and
                    // `render_th`. Taking the library's as well indents every
                    // value twice and leaves a column's width unknowable here.
                    .p_0()
            })
            .collect();

        Self {
            columns,
            sort: Vec::new(),
            sortable: false,
            result,
            display,
            active: None,
            pending: Vec::new(),
            editing: None,
            captured: None,
            restored_total: None,
            foreign_keys: Vec::new(),
            follow_groups: Vec::new(),
        }
    }

    /// The sort the statement asked the server for, so the headers can say
    /// which columns the rows are ordered by and in which direction, and
    /// whether asking for another one is possible at all.
    pub fn with_sort(mut self, sort: Vec<(usize, bool)>, sortable: bool) -> Self {
        self.sort = sort;
        self.sortable = sortable;
        self
    }

    /// A grid read back from a snapshot.
    ///
    /// There is no `QueryResult` behind it: the snapshot keeps column names and
    /// values, not the type information or the edit target those come with. So
    /// a restored grid shows rows, and the inspector and in-grid editing come
    /// back with the run that replaces it.
    pub fn restored(stored: &StoredGrid) -> Self {
        let mut grid = Self::new(QueryResult {
            columns: stored
                .columns
                .iter()
                .map(|name| db::Column {
                    name: name.clone(),
                    data_type: None,
                })
                .collect(),
            rows: stored.rows.clone(),
            ..QueryResult::default()
        });

        // Over the widths `new` just fitted, which measured the capped rows
        // rather than the layout the user was actually looking at.
        for (column, width) in grid.columns.iter_mut().zip(&stored.widths) {
            column.width = px(*width);
        }
        // The headers say what the rows are ordered by. `sortable` stays false:
        // whether a header click can do anything is a fact about the statement,
        // and a snapshot is not a statement -- the next run settles it.
        grid.sort = stored.sort.clone();
        let (rows, columns) = (grid.result.rows.len(), grid.columns.len());
        // A snapshot is capped, so the cell that was active may be past the
        // rows that came back with it -- and that is not a cell.
        grid.active = stored
            .active
            .filter(|(row, col)| *row < rows && *col < columns);
        grid.captured = Some(stored.captured);
        grid.restored_total = Some(stored.total_rows);
        grid
    }

    /// When a restored grid's rows were snapshotted, or `None` for rows a run
    /// put here.
    pub fn captured(&self) -> Option<u64> {
        self.captured
    }

    /// How many rows the result behind this grid had. More than the grid holds
    /// only for a restored snapshot the cap trimmed -- which is the one case
    /// where the rows on screen are not the whole result set.
    pub fn total_rows(&self) -> usize {
        self.restored_total.unwrap_or(self.result.rows.len())
    }

    /// What a snapshot of this grid keeps. The tab's own fields -- the
    /// statement, the row limit -- are the caller's to fill in: the grid does
    /// not know which kind of tab it is in.
    ///
    /// `pending` and `editing` are deliberately absent. An unapplied edit is
    /// against rows this session fetched, and a restored grid is not those rows.
    pub fn stored(&self) -> StoredGrid {
        StoredGrid {
            columns: self
                .result
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect(),
            // Capped here rather than in `write_grid`, which would have to
            // clone the whole vector to keep the front of it.
            rows: self
                .result
                .rows
                .iter()
                .take(GRID_ROW_CAP)
                .cloned()
                .collect(),
            // The result's own size, which a restored grid knows and does not
            // hold: recomputing it from the capped rows is what collapsed a
            // 20,000-row snapshot to 5,000 on the next save.
            total_rows: self.total_rows(),
            sort: self.sort.clone(),
            order_by: Vec::new(),
            widths: self
                .columns
                .iter()
                .map(|column| f32::from(column.width))
                .collect(),
            active: self.active,
            last_query: None,
            limit: None,
            filter: String::new(),
            showing_structure: false,
            // A snapshot that is written back unchanged keeps its own age: it
            // is still the rows it was, and restamping it would make every
            // restart claim the cache was just taken.
            captured: self.captured.unwrap_or_else(captured_at),
        }
    }

    /// The widths the table is drawing, which is where a drag lands: the
    /// library resizes its own copy of the columns, so without this a snapshot
    /// would keep the width every column was first fitted to.
    pub fn set_widths(&mut self, widths: &[gpui::Pixels]) {
        for (column, width) in self.columns.iter_mut().zip(widths) {
            column.width = *width;
        }
    }

    pub fn columns(&self) -> &[crate::db::Column] {
        &self.result.columns
    }

    /// Record which of this result's columns carry a foreign key, given the
    /// column names the relation's structure says do.
    ///
    /// ponytail: matched by name, not by position. A preview is `SELECT *` of
    /// one relation, so its column names are the relation's; an aliased or
    /// computed projection therefore matches nothing and offers no arrow. That
    /// is the right failure — an arrow placed by position would filter the
    /// referenced relation by a value from some other column.
    pub fn mark_foreign_keys(&mut self, columns: &[String]) {
        self.foreign_keys = self
            .result
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| columns.contains(&column.name))
            .map(|(index, _)| index)
            .collect();
        self.follow_groups = self
            .foreign_keys
            .iter()
            .map(|col| SharedString::from(format!("follow-key-{col}")))
            .collect();
    }

    /// The hover group a key column's cells share, and `None` for a column
    /// nothing can be followed from.
    fn follow_group(&self, col: usize) -> Option<&SharedString> {
        let key = self.foreign_keys.iter().position(|key| *key == col)?;
        self.follow_groups.get(key)
    }

    /// Whether this column's cells can be followed to the row they reference.
    pub fn follows_a_key(&self, col: usize) -> bool {
        self.foreign_keys.contains(&col)
    }

    /// The whole value behind a cell, not the clipped one the grid paints: a
    /// column is a couple of hundred pixels wide and a JSONB document is not,
    /// and copying what happens to fit would be the same bug as reading a value
    /// through the column.
    fn cell(&self, row_ix: usize, col_ix: usize) -> Option<&str> {
        self.result.rows.get(row_ix)?.get(col_ix)?.as_deref()
    }

    /// Every column of one row, named, typed where the type is known, and
    /// carrying the value itself rather than the string the column had room
    /// for. This is what the row inspector reads.
    pub fn fields(&self, row_ix: usize) -> Vec<Field> {
        if row_ix >= self.result.rows.len() {
            return Vec::new();
        }

        self.result
            .columns
            .iter()
            .enumerate()
            .map(|(col_ix, column)| Field {
                name: column.name.clone().into(),
                data_type: column.data_type.clone().map(SharedString::from),
                value: self
                    .cell(row_ix, col_ix)
                    .map(|value| clip_to(value, FIELD_DISPLAY_LIMIT).into()),
            })
            .collect()
    }
}

/// The editing half of the grid: what the user has changed, and not one
/// statement of SQL. Generating and running that is the workspace's job, which
/// is why every one of these is computable without a window.
impl ResultGrid {
    /// The cell `Enter` acts on, if the user has reached one. `None` on a
    /// result set nobody has touched yet, and on every new one.
    pub fn active(&self) -> Option<(usize, usize)> {
        self.active
    }

    /// What the server returned, whole: the rows an export writes out, and not
    /// the clipped `display` strings the columns had room for. Pending edits are
    /// not folded in, because this is the result set, not the grid's view of it.
    pub fn result(&self) -> &QueryResult {
        &self.result
    }

    /// The whole value behind the active cell, which is what `cmd+c` copies —
    /// not the clipped string the column had room for. `None` while an input is
    /// open, because there `cmd+c` is the input's own text selection, and on a
    /// NULL, which is an absent value rather than the word painted for one.
    pub fn active_value(&self) -> Option<&str> {
        if self.editing.is_some() {
            return None;
        }
        let (row, col) = self.active?;
        self.cell(row, col)
    }

    /// Fold the library's row selection into the active cell. Its own arrow-key
    /// actions move that selection, so folding the event here is what makes
    /// them move the ring, and there is no competing binding to fight.
    ///
    /// The column is kept: moving down a column is not moving out of it. With
    /// nothing active yet the first column is the origin, because a keystroke
    /// on a grid has to leave the ring somewhere readable.
    pub fn select_row(&mut self, row: usize) {
        self.set_active(row, self.active.map_or(0, |(_, col)| col));
    }

    /// The same fold for a column change, keeping the row. A header click lands
    /// here too, and is deliberately not special-cased: on a sortable result
    /// the sort re-runs and replaces this delegate wholesale, so the ring is
    /// dropped either way, and on one that cannot be sorted the ring sitting at
    /// the top of the column the user just pointed at is where their last
    /// action was.
    pub fn select_col(&mut self, col: usize) {
        self.set_active(self.active.map_or(0, |(row, _)| row), col);
    }

    /// Move the ring to a cell. An input open on some other cell goes with it:
    /// one cell holding a focused input while another wears the ring `Enter`
    /// follows is two cells claiming the keyboard.
    fn set_active(&mut self, row: usize, col: usize) {
        if self
            .editing
            .as_ref()
            .is_some_and(|editing| (editing.row, editing.col) != (row, col))
        {
            self.editing = None;
        }
        self.active = Some((row, col));
    }

    /// Whether this cell can be written back at all: the result has to be
    /// traceable to one table, the column has to exist in it, and it must not
    /// be part of the key — a key edit is the one edit whose result cannot be
    /// re-verified afterwards, so the spec's §3 refuses it.
    ///
    /// Nor a binary column. What the grid shows there is a blob *literal* the
    /// user could not type a replacement for anyway, and the value that came
    /// back would be written as the text it looks like — see
    /// [`db::is_binary_type`].
    pub fn editable(&self, row: usize, col: usize) -> bool {
        let Some(edit) = &self.result.edit else {
            return false;
        };
        row < self.result.rows.len()
            && edit.columns.get(col).is_some_and(Option::is_some)
            && !edit.keys.contains(&col)
            && !self
                .result
                .columns
                .get(col)
                .and_then(|column| column.data_type.as_deref())
                .is_some_and(db::is_binary_type)
    }

    /// The row's table and its whole primary key, named and valued, or nothing.
    ///
    /// The same question [`ResultGrid::editable`] asks, answered for a whole row
    /// instead of one cell, because deleting a row and editing a cell of it need
    /// exactly the same thing: a predicate that reaches this row and no other.
    /// `None` where the result traces to no table, where the index has outlived
    /// the rows, and where any key column came back NULL — `=` does not find a
    /// NULL, so a predicate built from one reaches nothing.
    pub fn row_key(&self, row: usize) -> Option<RowKey> {
        let edit = self.result.edit.as_ref()?;
        if row >= self.result.rows.len() {
            return None;
        }
        Some((
            edit.schema.clone(),
            edit.table.clone(),
            self.key_values(edit, row)?,
        ))
    }

    /// Open an input on a cell. `false` when the cell is not editable, and
    /// nothing at all happens then: the notice belongs to `main.rs`.
    pub fn begin_edit(&mut self, row: usize, col: usize) -> bool {
        if !self.editable(row, col) {
            return false;
        }
        self.editing = Some(Editing {
            row,
            col,
            input: None,
        });
        true
    }

    /// Abandon the open input, leaving nothing behind.
    pub fn cancel_edit(&mut self) {
        self.editing = None;
    }

    /// Record a new value for a cell, where `None` is a `NULL`. `false` when
    /// the cell is not editable, in which case nothing is recorded.
    pub fn set_pending(&mut self, row: usize, col: usize, value: Option<String>) -> bool {
        if !self.editable(row, col) {
            return false;
        }

        // An input seeds itself with the value as fetched, so opening an edit
        // and closing it without typing arrives here carrying the server's own
        // value back. That is not an edit, and recording it would write a
        // rendered value over the value it was rendered from. Nulling a cell
        // the server already left NULL is the same non-edit, which comparing
        // the absences rather than their renderings is what catches.
        if self.cell(row, col) == value.as_deref() {
            self.pending
                .retain(|edit| (edit.row, edit.col) != (row, col));
            return true;
        }

        let value: Option<SharedString> = value.map(SharedString::from);
        // Bytes, not characters: it only has to be cheap and never under-count,
        // and `clip` is a no-op on anything that turns out to fit.
        let shown = value
            .as_ref()
            .map(|value| match value.len() > CELL_DISPLAY_LIMIT {
                true => SharedString::from(clip(value)),
                false => value.clone(),
            });
        match self
            .pending
            .iter_mut()
            .find(|edit| edit.row == row && edit.col == col)
        {
            Some(edit) => {
                edit.value = value;
                edit.shown = shown;
            }
            None => self.pending.push(PendingEdit {
                row,
                col,
                value,
                shown,
            }),
        }
        true
    }

    /// Stage a `NULL` on a cell, closing any input open over it. `false` when
    /// the cell is not editable, and nothing happens then.
    ///
    /// The one implementation behind both gestures. The editor's `NULL` button
    /// dispatches the same action the palette and the keystroke do, so there is
    /// nothing here that can behave differently depending on which was used.
    pub fn set_null(&mut self, row: usize, col: usize) -> bool {
        if !self.set_pending(row, col, None) {
            return false;
        }
        // An input still holding the old text would commit it back on the next
        // `Enter`, over the NULL that was just asked for.
        if self
            .editing
            .as_ref()
            .is_some_and(|editing| (editing.row, editing.col) == (row, col))
        {
            self.editing = None;
        }
        true
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Back to exactly what the server returned.
    pub fn discard_pending(&mut self) {
        self.pending.clear();
        self.editing = None;
    }

    /// One entry per changed row, in the order the rows were first edited, with
    /// every changed column of that row in a single `SET`.
    ///
    /// A row whose key is not fully readable is dropped rather than guessed at:
    /// a `NULL` in a key column, or a key column the result set does not carry,
    /// leaves Slate unable to name the row.
    pub fn pending_updates(&self) -> Vec<PendingRow> {
        let Some(edit) = &self.result.edit else {
            return Vec::new();
        };

        let mut rows: Vec<usize> = Vec::new();
        for row in self.pending.iter().map(|edit| edit.row) {
            if !rows.contains(&row) {
                rows.push(row);
            }
        }

        rows.into_iter()
            .filter_map(|row| {
                let sets: Vec<(String, Option<String>)> = self
                    .pending
                    .iter()
                    .filter(|pending| pending.row == row)
                    .filter_map(|pending| {
                        let name = edit.columns.get(pending.col)?.clone()?;
                        Some((name, pending.value.as_ref().map(SharedString::to_string)))
                    })
                    .collect();
                if sets.is_empty() {
                    return None;
                }
                Some(PendingRow {
                    schema: edit.schema.clone(),
                    table: edit.table.clone(),
                    sets,
                    keys: self.key_values(edit, row)?,
                })
            })
            .collect()
    }

    /// The row's key columns, named and carrying the value the server sent.
    /// `None` if any of them is missing, which is what refuses the whole row.
    fn key_values(&self, edit: &EditTarget, row: usize) -> Option<Vec<(String, String)>> {
        edit.keys
            .iter()
            .map(|&col| {
                let name = edit.columns.get(col)?.clone()?;
                Some((name, self.cell(row, col)?.to_string()))
            })
            .collect()
    }

    fn pending_at(&self, row: usize, col: usize) -> Option<&PendingEdit> {
        self.pending
            .iter()
            .find(|edit| edit.row == row && edit.col == col)
    }

    /// The input for the cell being edited, created on its first render. `None`
    /// for every other cell, which is every cell but one.
    fn editing_input(
        &mut self,
        row: usize,
        col: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Entity<InputState>> {
        let editing = self.editing.as_ref()?;
        if (editing.row, editing.col) != (row, col) {
            return None;
        }
        if let Some(input) = &editing.input {
            return Some(input.clone());
        }

        // The whole value, not the clipped one: this is the value being
        // changed. A NULL seeds as the empty string because that is the only
        // thing an input can hold; typing one back is the `SetNull` action's
        // job, not the input's (spec §3).
        let seed = match self.pending_at(row, col) {
            Some(pending) => pending.value.clone().unwrap_or_default(),
            None => self
                .cell(row, col)
                .map(|value| SharedString::from(value.to_string()))
                .unwrap_or_default(),
        };
        let input = cx.new(|cx| InputState::new(window, cx));
        // `set_value` rather than `default_value`: only the former leaves the
        // caret after the text, and an edit starts from the end of the value.
        input.update(cx, |input, cx| input.set_value(seed, window, cx));
        // The keystrokes that follow belong to the value rather than to the
        // grid's selection, so the input takes focus as it appears.
        input.focus_handle(cx).focus(window);
        self.editing.as_mut()?.input = Some(input.clone());
        Some(input)
    }

    /// Take the open input's value into the pending set.
    fn commit_edit(&mut self, cx: &App) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        let Some(input) = editing.input else {
            return;
        };
        self.set_pending(
            editing.row,
            editing.col,
            Some(input.read(cx).value().to_string()),
        );
    }
}

/// One column of one row, for the inspector panel.
pub struct Field {
    pub name: SharedString,
    /// The server's own name for the column's type, when Slate could learn it
    /// without running the statement twice.
    pub data_type: Option<SharedString>,
    pub value: Option<SharedString>,
}

/// A column wide enough for what it holds. One fixed width for every column
/// wastes the screen on a boolean and hides most of a UUID; a table's own
/// shape is the only thing that knows how wide its columns want to be.
fn fitted_width(name: &str, display: &[Vec<Option<SharedString>>], col_ix: usize) -> gpui::Pixels {
    let widest = display
        .iter()
        .take(WIDTH_SAMPLE_ROWS)
        .filter_map(|row| row.get(col_ix))
        // A NULL still paints a word, so an all-NULL column is not zero wide.
        .map(|cell| {
            cell.as_ref()
                .map_or(NULL_LABEL.len(), |value| value.chars().count())
        })
        .max()
        .unwrap_or(0);

    let values = widest as f32 * CHAR_WIDTH;
    let header = name.chars().count() as f32 * CHAR_WIDTH + HEADER_CONTROL_WIDTH;
    let padding = 2.0 * layout::SPACE_SM;

    px((values.max(header) + padding).clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH))
}

fn clip(value: &str) -> String {
    clip_to(value, CELL_DISPLAY_LIMIT)
}

/// Cut to a character count, never a byte count: slicing bytes panics in the
/// middle of a codepoint, and the values here are arbitrary user data.
fn clip_to(value: &str, limit: usize) -> String {
    match value.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &value[..end]),
        None => value.to_string(),
    }
}

impl TableDelegate for ResultGrid {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.result.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> &Column {
        &self.columns[col_ix]
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let (muted, faint, text) = {
            let t = theme(cx);
            (t.text_muted, t.text_faint, t.text)
        };
        let key = self
            .sort
            .iter()
            .position(|(column, _)| *column == col_ix)
            .map(|position| (position, self.sort[position].1));

        let base = div()
            .id(("column-header", col_ix))
            .h_full()
            // Not `size_full`: the header cell clips, and the resize handle
            // lives at its trailing edge.
            .flex_1()
            .min_w_0()
            .px(px(layout::SPACE_SM))
            .flex()
            .items_center()
            .gap(px(layout::SPACE_XS))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(layout::TEXT_SM))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(match key.is_some() {
                true => text,
                false => muted,
            });

        base.child(
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .child(self.columns[col_ix].name.clone()),
        )
        .child(
            div()
                .ml_auto()
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(2.))
                .map(|control| match key {
                    Some((_, ascending)) => control.child(
                        icon(match ascending {
                            true => icon::SORT_UP,
                            false => icon::SORT_DOWN,
                        })
                        .size(px(12.))
                        .text_color(text),
                    ),
                    // Faint rather than absent: a header that shows nothing
                    // until it is clicked does not read as clickable. And
                    // absent rather than faint where a click would do
                    // nothing, which is the same rule the other way round.
                    None => control.children(
                        self.sortable
                            .then(|| icon(icon::SORTABLE).size(px(12.)).text_color(faint)),
                    ),
                })
                // Only worth saying which key this is when there is more
                // than one of them.
                .children(key.filter(|_| self.sort.len() > 1).map(|(position, _)| {
                    div()
                        .text_size(px(layout::TEXT_XS))
                        .text_color(muted)
                        .child((position + 1).to_string())
                })),
        )
        // The click goes to the workspace, which owns the statement: a sort
        // is a change to the SQL and a re-run, not a reordering of rows the
        // grid happens to be holding.
        .on_click(cx.listener(move |_, _, window, cx| {
            window.dispatch_action(Box::new(crate::SortColumn { column: col_ix }), cx);
        }))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let (text, faint, edited_bg, active_ring) = {
            let t = theme(cx);
            (t.text, t.text_faint, t.edited, t.accent)
        };
        let base = div()
            .id(("cell", row_ix * self.columns.len() + col_ix))
            .size_full()
            .px(px(layout::SPACE_SM))
            // A ring rather than a wash: the pending-edit wash is taken, and
            // the active cell has to be distinguishable while wearing it.
            // Unconditional width so the ring appearing costs the row no
            // reflow -- gpui lays a border out whether or not there is a
            // colour to paint it with.
            .border_1()
            .when(self.active == Some((row_ix, col_ix)), |cell| {
                cell.border_color(active_ring)
            })
            .flex()
            .items_center();

        if let Some(input) = self.editing_input(row_ix, col_ix, window, cx) {
            return base
                .bg(edited_bg)
                .gap(px(layout::SPACE_SM))
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&input)
                            // The cell is the frame; a second border and
                            // background inside one would read as a control in
                            // a hole.
                            .appearance(false)
                            .px_0()
                            // The cell is the frame, so take its height rather
                            // than the control's own `rems`-based one, which is
                            // sized for a standalone field and overflows the
                            // row.
                            .h_full()
                            .text_size(px(layout::TEXT_MD)),
                    ),
                )
                // How the action is found. It dispatches rather than nulling the
                // cell itself, so the button and the keystroke cannot drift.
                .child(
                    div()
                        .id(("null", row_ix * self.columns.len() + col_ix))
                        .flex_shrink_0()
                        .italic()
                        .text_color(faint)
                        .child(NULL_LABEL)
                        .on_click(cx.listener(move |_, _, window, cx| {
                            window.dispatch_action(Box::new(crate::SetNull), cx);
                        })),
                )
                // The input has focus, so both keystrokes arrive here on their
                // way out of it. Consumed rather than propagated: `escape`
                // otherwise reaches the workspace and moves focus to the editor.
                .on_action(cx.listener(
                    move |table, _: &gpui_component::input::Enter, window, cx| {
                        table.delegate_mut().commit_edit(cx);
                        table.focus_handle(cx).focus(window);
                        cx.stop_propagation();
                        cx.notify();
                    },
                ))
                .on_action(cx.listener(
                    move |table, _: &gpui_component::input::Escape, window, cx| {
                        table.delegate_mut().cancel_edit();
                        table.focus_handle(cx).focus(window);
                        cx.stop_propagation();
                        cx.notify();
                    },
                ));
        }

        // A pending value is painted from the pending set rather than by
        // patching `display`, which stays exactly as fetched.
        let pending = self.pending_at(row_ix, col_ix);
        let cell = match pending {
            // A staged NULL has no `shown`, so it falls into the same italic
            // branch a fetched one does while still wearing the edited wash.
            Some(pending) => pending.shown.clone(),
            // Rows are not guaranteed rectangular and an index can outlive the
            // result set it was taken from. Indexing here would abort the
            // process mid-paint and take the user's editor buffer with it.
            None => self
                .display
                .get(row_ix)
                .and_then(|row| row.get(col_ix))
                .and_then(Option::as_ref)
                .cloned(),
        };

        let follows_a_key = self.follows_a_key(col_ix)
            // A NULL references nothing, so there is nothing to follow it to.
            && self.cell(row_ix, col_ix).is_some();
        let group = follows_a_key
            .then(|| self.follow_group(col_ix).cloned())
            .flatten();
        // Faint on hover over any cell of the column, and always on the active
        // one, so the gesture is reachable from the keyboard as well as the
        // mouse. Present either way rather than added on hover: a cell that
        // reflows under the pointer is a cell that moves as it is read.
        let active = self.active == Some((row_ix, col_ix));

        base.overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_color(if cell.is_some() { text } else { faint })
            // Italic so a NULL cannot be mistaken for the four-letter string.
            .when(cell.is_none(), |cell| cell.italic())
            .when(pending.is_some(), |cell| cell.bg(edited_bg))
            .child(cell.unwrap_or(NULL_LABEL))
            // The workspace owns the statement and the tabs and the grid owns
            // neither, so this leaves exactly as a header's sort click does.
            .when_some(group.clone(), |cell, group| cell.group(group))
            .children(group.map(|group| {
                div()
                    .id(("follow-key", row_ix * self.columns.len() + col_ix))
                    .ml_auto()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .text_color(faint)
                    .when(!active, |hidden| {
                        hidden
                            .opacity(0.)
                            .group_hover(group, |shown| shown.opacity(1.))
                    })
                    .child(icon(icon::FOLLOW_KEY).size(px(12.)))
                    .on_click(cx.listener(move |_, _, window, cx| {
                        // Or the cell underneath takes the click as a move of
                        // the ring it is already on.
                        cx.stop_propagation();
                        window.dispatch_action(Box::new(crate::FollowForeignKey), cx);
                    }))
            }))
            // No fallback: `begin_edit` already refuses silently on a cell
            // that cannot be written, which is the right outcome here too --
            // a double click on a read-only cell does nothing rather than
            // copying, because copying is `cmd+c` on every cell alike.
            .on_double_click(cx.listener(move |table, _, _, cx| {
                if table.delegate_mut().begin_edit(row_ix, col_ix) {
                    cx.notify();
                }
            }))
            // What `Enter` will act on. The library records the row of a cell
            // click and never the column, so the coordinate is set here whole.
            // The row it does record arrives as `SelectRow` and folds back in
            // keeping this column, so the two orders converge on the same cell.
            // Runs on the first click of a double click too, and
            // `set_active` on the cell an editor just opened on is a no-op --
            // same coordinates, so `editing` survives -- which is what keeps
            // the second listener from closing what the first just opened.
            .on_click(cx.listener(move |table, _, _, cx| {
                table.delegate_mut().set_active(row_ix, col_ix);
                cx.notify();
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Column as DbColumn;

    fn column(name: &str) -> DbColumn {
        DbColumn {
            name: name.into(),
            data_type: None,
        }
    }

    /// A grid over one column of cells, which is enough to order rows by.
    fn grid_of(values: &[Option<&str>]) -> ResultGrid {
        ResultGrid::new(QueryResult {
            columns: vec![column("a")],
            rows: values
                .iter()
                .map(|value| vec![value.map(str::to_string)])
                .collect(),
            ..QueryResult::default()
        })
    }

    /// A grid over `id, note, total` where `id` is the key, `note` is an alias
    /// for the real column `body`, and `total` is computed. One column of each
    /// kind that editing has to tell apart.
    fn editable_grid() -> ResultGrid {
        ResultGrid::new(QueryResult {
            columns: vec![column("id"), column("note"), column("total")],
            rows: vec![
                vec![Some("7".into()), Some("first".into()), Some("1".into())],
                vec![Some("8".into()), Some("second".into()), Some("2".into())],
            ],
            edit: Some(EditTarget {
                schema: "public".into(),
                table: "measurements".into(),
                columns: vec![Some("id".into()), Some("body".into()), None],
                keys: vec![0],
            }),
            ..QueryResult::default()
        })
    }

    /// The same shape as [`editable_grid`], but with the editable column left
    /// NULL by the server: the one row a staged NULL has to be told apart from.
    fn grid_with_a_null() -> ResultGrid {
        ResultGrid::new(QueryResult {
            columns: vec![column("id"), column("note")],
            rows: vec![vec![Some("7".into()), None]],
            edit: Some(EditTarget {
                schema: "public".into(),
                table: "measurements".into(),
                columns: vec![Some("id".into()), Some("body".into())],
                keys: vec![0],
            }),
            ..QueryResult::default()
        })
    }

    /// The same shape again, but the NULL is in the *key* column: the row the
    /// server left unnameable.
    fn grid_with_a_null_key() -> ResultGrid {
        ResultGrid::new(QueryResult {
            columns: vec![column("id"), column("note")],
            rows: vec![vec![None, Some("first".into())]],
            edit: Some(EditTarget {
                schema: "public".into(),
                table: "measurements".into(),
                columns: vec![Some("id".into()), Some("body".into())],
                keys: vec![0],
            }),
            ..QueryResult::default()
        })
    }

    #[test]
    fn a_row_offers_its_whole_key_or_nothing_at_all() {
        // The same condition that makes a cell editable, because it is the same
        // question: can Slate name this row.
        let grid = editable_grid();
        assert_eq!(
            grid.row_key(1),
            Some((
                "public".to_string(),
                "measurements".to_string(),
                vec![("id".to_string(), "8".to_string())]
            ))
        );
        // A row index can outlive the rows it was taken from.
        assert_eq!(grid.row_key(9), None);
        // And a result Slate cannot trace to one table has no key anywhere.
        assert_eq!(grid_of(&[Some("x")]).row_key(0), None);
    }

    #[test]
    fn a_row_whose_key_the_server_left_null_cannot_be_named() {
        // `=` does not find a NULL, so a predicate built from one matches
        // nothing -- and a delete that matches nothing is not the delete the
        // confirmation described.
        assert_eq!(grid_with_a_null_key().row_key(0), None);
        // The same row is nameable when it is a non-key column that is NULL.
        assert!(grid_with_a_null().row_key(0).is_some());
    }

    #[test]
    fn a_pending_edit_leaves_the_fetched_rows_alone() {
        // The grid shows changed-against-server by holding both. Writing the
        // edit into `result.rows` would lose the server's value for good.
        let mut grid = editable_grid();
        assert!(grid.set_pending(0, 1, Some("changed".into())));

        assert_eq!(grid.result.rows[0][1].as_deref(), Some("first"));
        assert_eq!(
            grid.display[0][1].as_ref().map(SharedString::as_ref),
            Some("first")
        );
        assert_eq!(grid.cell(0, 1), Some("first"));
        assert!(grid.has_pending());
    }

    #[test]
    fn several_edits_on_one_row_become_one_update() {
        // One statement per row, not per cell: two `UPDATE`s against the same
        // key would be two round trips writing over each other's work.
        let mut grid = editable_grid();
        assert!(grid.set_pending(0, 1, Some("once".into())));
        assert!(grid.set_pending(0, 1, Some("twice".into())));

        let updates = grid.pending_updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].schema, "public");
        assert_eq!(updates[0].table, "measurements");
        // The alias is resolved here, so the caller generates SQL against the
        // column the table actually has.
        assert_eq!(
            updates[0].sets,
            vec![("body".to_string(), Some("twice".to_string()))]
        );
    }

    #[test]
    fn two_edited_rows_become_two_updates() {
        let mut grid = editable_grid();
        assert!(grid.set_pending(1, 1, Some("later".into())));
        assert!(grid.set_pending(0, 1, Some("earlier".into())));

        let updates = grid.pending_updates();
        assert_eq!(updates.len(), 2);
        // In the order they were edited, so the batch reads the way it was made.
        assert_eq!(updates[0].keys, vec![("id".to_string(), "8".to_string())]);
        assert_eq!(updates[1].keys, vec![("id".to_string(), "7".to_string())]);
    }

    #[test]
    fn a_grid_survives_the_round_trip_through_a_snapshot() {
        // What reopening a profile shows. Every field here is one a person can
        // see is missing: a column that came back narrow, the sort arrows gone,
        // the ring on another cell.
        let mut grid = editable_grid();
        grid.sort = vec![(2, false), (0, true)];
        grid.set_widths(&[px(120.), px(64.), px(200.)]);
        grid.set_active(1, 2);
        // An edit nobody applied stays with the session that typed it.
        assert!(grid.set_pending(0, 1, Some("changed".into())));

        let restored = ResultGrid::restored(&grid.stored());

        assert_eq!(
            restored
                .columns()
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["id", "note", "total"]
        );
        assert_eq!(
            restored
                .columns
                .iter()
                .map(|column| column.width)
                .collect::<Vec<_>>(),
            [px(120.), px(64.), px(200.)]
        );
        assert_eq!(restored.result.rows, grid.result.rows);
        assert_eq!(restored.sort, vec![(2, false), (0, true)]);
        assert_eq!(restored.active, Some((1, 2)));
        assert!(!restored.has_pending());
        // The rows the cache holds are the rows the status bar counts.
        assert_eq!(grid.stored().total_rows, 2);
    }

    #[test]
    fn a_capped_snapshot_keeps_the_result_size_across_a_re_save() {
        // Restore, then quit without re-running: the snapshot is written back
        // from a grid holding `GRID_ROW_CAP` rows, and recomputing the count
        // from those would collapse the real size to the cap for good.
        let restored = ResultGrid::restored(&StoredGrid {
            columns: vec!["n".into()],
            rows: (0..GRID_ROW_CAP)
                .map(|n| vec![Some(n.to_string())])
                .collect(),
            total_rows: 20_000,
            sort: Vec::new(),
            order_by: Vec::new(),
            widths: Vec::new(),
            active: None,
            last_query: None,
            limit: None,
            filter: String::new(),
            showing_structure: false,
            captured: 1_700_000_000,
        });

        assert_eq!(restored.total_rows(), 20_000);
        let written = restored.stored();
        assert_eq!(written.total_rows, 20_000);
        assert_eq!(written.rows.len(), GRID_ROW_CAP);
        // And again, however many times the profile is reopened.
        assert_eq!(ResultGrid::restored(&written).stored().total_rows, 20_000);
    }

    #[test]
    fn a_result_larger_than_the_cap_is_capped_once_by_the_grid() {
        // `write_grid` caps too, but only as a backstop: cloning the whole row
        // vector to keep the front of it is what this avoids.
        let grid = ResultGrid::new(QueryResult {
            columns: vec![column("n")],
            rows: (0..GRID_ROW_CAP + 10)
                .map(|n| vec![Some(n.to_string())])
                .collect(),
            ..QueryResult::default()
        });

        let written = grid.stored();
        assert_eq!(written.rows.len(), GRID_ROW_CAP);
        assert_eq!(written.total_rows, GRID_ROW_CAP + 10);
    }

    #[test]
    fn a_snapshots_active_cell_is_dropped_when_its_row_did_not_fit() {
        // `write_grid` caps the rows, so the cell that was active can be past
        // the end of what comes back -- and a ring around nothing is worse
        // than none.
        let grid = ResultGrid::restored(&StoredGrid {
            active: Some((9_000, 0)),
            ..editable_grid().stored()
        });

        assert_eq!(grid.active, None);
    }

    #[test]
    fn a_key_travels_as_the_value_the_server_sent() {
        // The `WHERE` names the row the server holds. Taking a key value from
        // the pending set would build a predicate that matches nothing.
        let mut grid = editable_grid();
        assert!(grid.set_pending(0, 1, Some("changed".into())));

        let updates = grid.pending_updates();
        assert_eq!(updates[0].keys, vec![("id".to_string(), "7".to_string())]);
        assert_eq!(
            updates[0].sets,
            vec![("body".to_string(), Some("changed".to_string()))]
        );
    }

    #[test]
    fn a_row_slate_cannot_name_produces_no_statement() {
        // A NULL key value leaves no predicate to write, and a row updated by
        // guesswork is the failure this whole feature is built to avoid.
        let mut grid = ResultGrid::new(QueryResult {
            columns: vec![column("id"), column("note")],
            rows: vec![vec![None, Some("orphan".into())]],
            edit: Some(EditTarget {
                schema: "public".into(),
                table: "measurements".into(),
                columns: vec![Some("id".into()), Some("body".into())],
                keys: vec![0],
            }),
            ..QueryResult::default()
        });

        assert!(grid.set_pending(0, 1, Some("changed".into())));
        assert!(grid.pending_updates().is_empty());
    }

    #[test]
    fn a_primary_key_column_cannot_be_edited() {
        // `SET id = new WHERE id = old` is the one edit whose result cannot be
        // re-verified afterwards -- spec §3.
        let mut grid = editable_grid();

        assert!(!grid.editable(0, 0));
        assert!(!grid.begin_edit(0, 0));
        assert!(!grid.set_pending(0, 0, Some("99".into())));
        assert!(!grid.has_pending());
    }

    #[test]
    fn a_computed_column_cannot_be_edited() {
        // There is no column behind it to write to.
        let mut grid = editable_grid();

        assert!(!grid.editable(0, 2));
        assert!(!grid.set_pending(0, 2, Some("99".into())));
        // Nor does a column past the end of the result set become editable.
        assert!(!grid.set_pending(0, 9, Some("99".into())));
        // Nor a row past the end of it.
        assert!(!grid.set_pending(9, 1, Some("99".into())));
        assert!(!grid.has_pending());
    }

    #[test]
    fn a_binary_column_cannot_be_edited() {
        // The grid paints a blob as the engine's own literal, and every value
        // written back goes through `quote_literal`, which would store the
        // literal as the text it looks like. Each engine's spelling of the
        // type, since one predicate answers for all three.
        for data_type in ["bytea", "blob", "longblob", "varbinary(16)", "BLOB"] {
            let mut grid = ResultGrid::new(QueryResult {
                columns: vec![
                    column("id"),
                    DbColumn {
                        name: "payload".into(),
                        data_type: Some(data_type.into()),
                    },
                ],
                rows: vec![vec![Some("7".into()), Some("x'AB'".into())]],
                edit: Some(EditTarget {
                    schema: "public".into(),
                    table: "measurements".into(),
                    columns: vec![Some("id".into()), Some("payload".into())],
                    keys: vec![0],
                }),
                ..QueryResult::default()
            });

            assert!(!grid.editable(0, 1), "{data_type}");
            assert!(!grid.begin_edit(0, 1), "{data_type}");
            assert!(!grid.set_pending(0, 1, Some("x'CD'".into())), "{data_type}");
            assert!(!grid.has_pending(), "{data_type}");
        }
    }

    #[test]
    fn a_value_that_did_not_change_records_nothing() {
        // An input seeds itself with the fetched value, so opening an edit and
        // pressing Enter arrives here with that value: two keystrokes must not
        // become a write.
        let mut grid = editable_grid();
        assert!(grid.set_pending(0, 1, Some("first".into())));
        assert!(!grid.has_pending());

        // Typed back to what the server sent, an edit already recorded goes.
        assert!(grid.set_pending(0, 1, Some("changed".into())));
        assert!(grid.has_pending());
        assert!(grid.set_pending(0, 1, Some("first".into())));
        assert!(!grid.has_pending());

        // Emptying a cell that holds text is still a deliberate edit.
        assert!(grid.set_pending(0, 1, Some(String::new())));
        assert_eq!(
            grid.pending_updates()[0].sets,
            vec![("body".to_string(), Some(String::new()))]
        );
    }

    #[test]
    fn a_staged_null_is_an_absence_and_not_the_empty_string() {
        let mut grid = editable_grid();

        assert!(grid.set_pending(0, 1, None));
        assert_eq!(
            grid.pending_updates()[0].sets,
            vec![("body".to_string(), None)]
        );

        assert!(grid.set_pending(0, 1, Some(String::new())));
        assert_eq!(
            grid.pending_updates()[0].sets,
            vec![("body".to_string(), Some(String::new()))]
        );

        // It paints the word a fetched NULL paints: the absent `shown` is what
        // sends it down the cell's own italic branch.
        assert!(grid.set_pending(0, 1, None));
        assert!(grid.pending_at(0, 1).unwrap().shown.is_none());
    }

    #[test]
    fn one_gesture_stages_a_null_and_closes_the_input_over_it() {
        // Both ways of asking end here: the action on the active cell, and the
        // editor's own affordance, which dispatches that same action rather
        // than doing this a second time.
        let mut grid = editable_grid();

        assert!(grid.begin_edit(0, 1));
        assert!(grid.set_null(0, 1));
        assert!(
            grid.editing.is_none(),
            "an input left open over a nulled cell would commit its text back"
        );
        assert_eq!(
            grid.pending_updates()[0].sets,
            vec![("body".to_string(), None)]
        );

        // A key column is no more nullable than it is editable, and the same
        // predicate refuses both.
        assert!(!grid.set_null(0, 0));
        assert!(!grid.set_null(0, 2));
    }

    #[test]
    fn the_empty_string_over_a_null_is_an_edit() {
        // It was not, while the two were the same three characters of SQL.
        // What now keeps an untouched input on a NULL from writing an empty
        // string over it is the editor -- `begin_edit` is a deliberate gesture
        // and `cancel_edit` records nothing -- not `set_pending`, which can no
        // longer tell a typed empty string from a seeded one and must not try.
        let mut grid = grid_with_a_null();

        assert!(grid.set_pending(0, 1, Some(String::new())));
        assert_eq!(
            grid.pending_updates()[0].sets,
            vec![("body".to_string(), Some(String::new()))]
        );
    }

    #[test]
    fn nulling_a_cell_the_server_already_left_null_records_nothing() {
        // Same reason an untouched value records nothing: this is not an edit.
        let mut grid = grid_with_a_null();

        assert!(grid.set_pending(0, 1, None));
        assert!(!grid.has_pending());
        assert!(grid.pending_updates().is_empty());
    }

    #[test]
    fn nothing_is_editable_without_an_edit_target() {
        // A join, an aggregate, or a select that dropped the key: `db` says the
        // rows cannot be addressed, and the grid stays read-only.
        let mut grid = grid_of(&[Some("x")]);

        assert!(!grid.editable(0, 0));
        assert!(!grid.begin_edit(0, 0));
        assert!(!grid.set_pending(0, 0, Some("y".into())));
        assert!(grid.pending_updates().is_empty());
    }

    #[test]
    fn discarding_leaves_the_grid_as_fetched() {
        // Discarding is dropping a collection, which is the whole reason the
        // fetched rows are never written.
        let mut grid = editable_grid();
        grid.set_pending(0, 1, Some("changed".into()));
        grid.begin_edit(1, 1);

        grid.discard_pending();

        assert!(!grid.has_pending());
        assert!(grid.editing.is_none());
        assert!(grid.pending_updates().is_empty());
        assert_eq!(grid.cell(0, 1), Some("first"));
    }

    #[test]
    fn a_long_pending_value_is_clipped_for_the_column_but_not_for_the_update() {
        // The same split as `display` against `cell`: the column paints what
        // fits, the statement carries the value.
        let value = "x".repeat(CELL_DISPLAY_LIMIT * 2);
        let mut grid = editable_grid();
        assert!(grid.set_pending(0, 1, Some(value.clone())));

        let pending = grid.pending_at(0, 1).unwrap();
        assert_eq!(
            pending.value.as_ref().map(SharedString::as_ref),
            Some(value.as_str())
        );
        assert_eq!(
            pending.shown.as_ref().unwrap().chars().count(),
            CELL_DISPLAY_LIMIT + 1
        );
        assert_eq!(grid.pending_updates()[0].sets[0].1, Some(value));
    }

    #[test]
    fn a_clicked_cell_is_remembered_as_the_active_one() {
        // gpui-component has no cell selection -- `set_selected_row` and
        // `set_selected_col` are mutually exclusive modes, not two halves of a
        // coordinate -- so if the grid forgets this, `Enter` has no target.
        let mut grid = editable_grid();
        assert!(grid.active().is_none());

        grid.set_active(1, 1);
        assert_eq!(grid.active(), Some((1, 1)));
        grid.set_active(0, 2);
        assert_eq!(grid.active(), Some((0, 2)));
    }

    #[test]
    fn enter_opens_an_input_on_the_active_cell_and_refuses_where_it_must() {
        // The two halves of the keystroke: the active cell is what `Enter`
        // acts on, and being active does not make an unwritable cell writable.
        let mut grid = editable_grid();

        grid.set_active(1, 1);
        let (row, col) = grid.active().unwrap();
        assert!(grid.begin_edit(row, col));
        assert!(grid.editing.is_some());

        grid.cancel_edit();
        grid.set_active(1, 0);
        let (row, col) = grid.active().unwrap();
        assert!(!grid.begin_edit(row, col));
        assert!(grid.editing.is_none());
    }

    #[test]
    fn an_input_open_elsewhere_closes_when_another_cell_becomes_active() {
        // Two cells claiming the keyboard -- one holding a focused input, the
        // other wearing the ring `Enter` follows -- is a grid nobody can read.
        let mut grid = editable_grid();
        grid.set_active(0, 1);
        assert!(grid.begin_edit(0, 1));

        grid.set_active(1, 1);

        assert!(grid.editing.is_none());
    }

    #[test]
    fn a_double_click_opens_the_editor_when_the_cell_allows_one() {
        // GPUI's click plumbing needs a window this module does not build, so
        // this drives `begin_edit`, the function the listener calls, rather
        // than the listener itself. Copy-on-double-click is gone -- `cmd+c`
        // covers it -- so `begin_edit`'s answer is the whole outcome: an
        // editable cell opens, the primary key column (read-only) stays closed.
        let mut grid = editable_grid();

        assert!(grid.begin_edit(0, 1));
        assert!(grid.editing.is_some());

        grid.cancel_edit();
        assert!(!grid.begin_edit(0, 0));
        assert!(grid.editing.is_none());
    }

    #[test]
    fn folding_a_selected_row_keeps_the_column_and_a_selected_column_keeps_the_row() {
        // The library moves a row *or* a column; the ring is a cell. If a fold
        // dropped the other half, an arrow key would send the ring back to the
        // first column or the first row instead of one cell over.
        let mut grid = editable_grid();
        grid.set_active(1, 2);

        grid.select_row(0);
        assert_eq!(grid.active(), Some((0, 2)));
        grid.select_col(1);
        assert_eq!(grid.active(), Some((0, 1)));
    }

    #[test]
    fn folding_with_nothing_active_yet_lands_on_a_cell_that_exists() {
        // The first arrow key of a session arrives with no ring on screen. A
        // half-coordinate is not a cell, so the missing half has to be an
        // origin rather than nothing at all.
        let mut grid = editable_grid();
        grid.select_row(1);
        assert_eq!(grid.active(), Some((1, 0)));

        let mut grid = editable_grid();
        grid.select_col(2);
        assert_eq!(grid.active(), Some((0, 2)));
    }

    #[test]
    fn a_click_and_the_selection_event_it_causes_converge_on_one_cell() {
        // A cell click sets the coordinate here and makes the library emit
        // `SelectRow` for the same row. Whichever arrives first, both have to
        // leave the ring on the clicked cell -- a listener order that decided
        // the answer would be the same two-notions-of-position bug again.
        let mut clicked_first = editable_grid();
        clicked_first.set_active(1, 2);
        clicked_first.select_row(1);

        let mut event_first = editable_grid();
        event_first.select_row(1);
        event_first.set_active(1, 2);

        assert_eq!(clicked_first.active(), Some((1, 2)));
        assert_eq!(event_first.active(), Some((1, 2)));
    }

    #[test]
    fn an_input_does_not_survive_the_ring_moving_off_its_cell() {
        // An arrow key routes through the same guard a click does. An input
        // holding focus on one cell while the ring sits on another is two cells
        // claiming the keyboard, and `Enter` acting on neither.
        let mut grid = editable_grid();
        grid.set_active(0, 1);
        assert!(grid.begin_edit(0, 1));

        grid.select_row(1);

        assert!(grid.editing.is_none());
        assert_eq!(grid.active(), Some((1, 1)));
    }

    #[test]
    fn a_copy_offers_the_whole_value_and_not_the_string_the_column_shows() {
        // `cmd+c` on a value wider than its column has to carry the value. The
        // clipped display string is what the previous copy gesture deliberately
        // did not read, and the reason it read the fetched row instead.
        let value = "x".repeat(CELL_DISPLAY_LIMIT * 2);
        let mut grid = ResultGrid::new(QueryResult {
            columns: vec![column("a"), column("b")],
            rows: vec![vec![Some(value.clone()), None]],
            ..QueryResult::default()
        });

        // Nothing active, nothing to copy.
        assert!(grid.active_value().is_none());

        grid.select_row(0);
        assert_eq!(grid.active_value(), Some(value.as_str()));
        assert_ne!(
            grid.display[0][0].as_ref().map(SharedString::as_ref),
            Some(value.as_str())
        );

        // A NULL is an absent value, not the word the grid paints for one.
        grid.select_col(1);
        assert!(grid.active_value().is_none());
    }

    #[test]
    fn a_copy_stays_out_of_the_way_of_an_open_input() {
        // Inside an input `cmd+c` is the text selection's. A grid copy firing
        // there would replace what the user just selected with the whole cell.
        let mut grid = editable_grid();
        grid.set_active(0, 1);
        assert_eq!(grid.active_value(), Some("first"));

        assert!(grid.begin_edit(0, 1));
        assert!(grid.active_value().is_none());

        grid.cancel_edit();
        assert_eq!(grid.active_value(), Some("first"));
    }

    #[test]
    fn a_read_only_cell_still_has_a_value_to_copy() {
        // The point of the gesture: a join, an aggregate or a key column can
        // never open an input, and every one of them has values worth copying.
        let mut keyless = grid_of(&[Some("joined")]);
        keyless.select_row(0);
        assert!(!keyless.editable(0, 0));
        assert_eq!(keyless.active_value(), Some("joined"));

        let mut grid = editable_grid();
        grid.set_active(0, 0);
        assert!(!grid.editable(0, 0));
        assert_eq!(grid.active_value(), Some("7"));
    }

    #[test]
    fn a_fresh_result_set_starts_with_no_active_cell() {
        // A coordinate outliving its rows is what `render_td`'s bounds checks
        // are about: it would put the ring on a cell nobody clicked and point
        // `Enter` at a row that no longer exists. Dropped with the delegate.
        let mut grid = editable_grid();
        grid.set_active(1, 1);

        let replaced = ResultGrid::new(QueryResult {
            columns: vec![column("a")],
            rows: vec![vec![Some("only".into())]],
            ..QueryResult::default()
        });

        assert!(replaced.active().is_none());
        // And an index that did outlive its rows opens nothing.
        assert!(!grid.begin_edit(9, 1));
    }

    /// A grid over `id, account_id, sku` — one key column between two that are
    /// not, so a mark by position would be visible as a mark on the wrong one.
    fn keyed_grid() -> ResultGrid {
        ResultGrid::new(QueryResult {
            columns: vec![column("id"), column("account_id"), column("sku")],
            rows: vec![vec![Some("7".into()), Some("42".into()), Some("x".into())]],
            ..QueryResult::default()
        })
    }

    #[test]
    fn only_the_columns_a_key_names_offer_to_follow_it() {
        let mut grid = keyed_grid();
        grid.mark_foreign_keys(&["account_id".to_string()]);

        assert!(!grid.follows_a_key(0));
        assert!(grid.follows_a_key(1));
        assert!(!grid.follows_a_key(2));
        // One hover group per key column, built here rather than per cell per
        // frame, and none at all for a column with nothing to follow.
        assert_eq!(grid.follow_group(0), None);
        assert_eq!(
            grid.follow_group(1).map(SharedString::as_ref),
            Some("follow-key-1")
        );
        assert_eq!(grid.follow_group(2), None);
    }

    #[test]
    fn a_key_naming_a_column_this_result_does_not_have_marks_nothing() {
        // The aliased-projection case: the structure names `account_id` and the
        // projection called it something else, so nothing is marked -- rather
        // than the column that happens to sit where `account_id` sat.
        let mut grid = keyed_grid();
        grid.mark_foreign_keys(&["owner_id".to_string()]);

        assert!((0..3).all(|col| !grid.follows_a_key(col)));
    }

    #[test]
    fn a_fresh_result_follows_nothing_until_the_structure_says_so() {
        // Every run replaces the delegate whole, so the marks are reapplied
        // from the structure rather than assumed to have survived.
        let mut grid = keyed_grid();
        grid.mark_foreign_keys(&["account_id".to_string()]);
        assert!(grid.follows_a_key(1));

        assert!(!keyed_grid().follows_a_key(1));
    }

    #[test]
    fn a_column_is_as_wide_as_what_it_holds() {
        let display = vec![
            vec![Some(SharedString::from("t")), Some("a longer value".into())],
            vec![Some("f".into()), Some("x".into())],
        ];
        let narrow = fitted_width("ok", &display, 0);
        let wide = fitted_width("note", &display, 1);

        assert!(narrow < wide, "{narrow:?} should be narrower than {wide:?}");
        // Clamped at both ends: a boolean column still has to be clickable,
        // and one JSONB document must not take the whole pane.
        assert!(f32::from(narrow) >= MIN_COLUMN_WIDTH);
        let document = vec![vec![Some(SharedString::from("y".repeat(9_000)))]];
        assert!(f32::from(fitted_width("x", &document, 0)) <= MAX_COLUMN_WIDTH);
    }

    #[test]
    fn a_row_reads_out_as_its_named_and_typed_fields() {
        let grid = ResultGrid::new(QueryResult {
            columns: vec![
                DbColumn {
                    name: "id".into(),
                    data_type: Some("int4".into()),
                },
                column("note"),
            ],
            rows: vec![vec![Some("7".into()), None]],
            ..QueryResult::default()
        });

        let fields = grid.fields(0);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "id");
        assert_eq!(
            fields[0].data_type.as_ref().map(SharedString::as_ref),
            Some("int4")
        );
        assert_eq!(
            fields[0].value.as_ref().map(SharedString::as_ref),
            Some("7")
        );
        // A NULL has to stay absent rather than becoming the word for one.
        assert!(fields[1].value.is_none());
        // A type Slate could not learn is shown as nothing, never as a guess.
        assert!(fields[1].data_type.is_none());
        // A selection can outlive the rows it was made against.
        assert!(grid.fields(4).is_empty());
    }

    #[test]
    fn the_inspector_shows_more_of_a_value_than_the_column_does() {
        let value = "x".repeat(FIELD_DISPLAY_LIMIT * 2);
        let grid = grid_of(&[Some(&value)]);
        let field = grid.fields(0).remove(0);
        let shown = field.value.unwrap();

        assert!(shown.chars().count() > CELL_DISPLAY_LIMIT);
        assert_eq!(shown.chars().count(), FIELD_DISPLAY_LIMIT + 1);
    }

    #[test]
    fn a_short_value_is_left_alone() {
        assert_eq!(clip("SELECT"), "SELECT");
    }

    #[test]
    fn clipping_never_splits_a_multibyte_character() {
        // Byte-slicing this at CELL_DISPLAY_LIMIT would panic mid-codepoint.
        let value = "🌍".repeat(CELL_DISPLAY_LIMIT * 2);
        let clipped = clip(&value);

        assert_eq!(clipped.chars().count(), CELL_DISPLAY_LIMIT + 1);
        assert!(clipped.ends_with('…'));
        assert!(clipped.chars().take(CELL_DISPLAY_LIMIT).all(|c| c == '🌍'));
    }

    #[test]
    fn a_copy_takes_the_whole_value_the_column_could_not_show() {
        let value = "x".repeat(CELL_DISPLAY_LIMIT * 3);
        let grid = ResultGrid::new(QueryResult {
            columns: vec![column("a")],
            rows: vec![vec![Some(value.clone())], vec![None]],
            ..QueryResult::default()
        });

        assert_eq!(grid.cell(0, 0), Some(value.as_str()));
        assert_ne!(
            grid.display[0][0].as_ref().map(SharedString::as_ref),
            Some(value.as_str())
        );
        // A NULL is an absent value, not the string the cell paints for one.
        assert_eq!(grid.cell(1, 0), None);
        assert_eq!(grid.cell(9, 9), None);
    }

    #[test]
    fn a_short_row_reads_as_absent_rather_than_panicking() {
        // A ragged result set must not be able to abort the render pass.
        let grid = ResultGrid::new(QueryResult {
            columns: vec![column("a"), column("b")],
            rows: vec![vec![Some("only one cell".into())]],
            ..QueryResult::default()
        });

        let row = &grid.display[0];
        assert!(!row.is_empty());
        assert!(row.get(1).is_none(), "row should be short, not padded");
    }
}
