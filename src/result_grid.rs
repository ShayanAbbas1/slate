use std::cmp::Ordering;

use gpui::{
    App, ClipboardItem, Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    InteractiveElementExt,
    table::{Column, ColumnSort, TableDelegate, TableState},
};

use crate::{
    db::QueryResult,
    theme::{layout, theme},
};

/// ponytail: a column is a few hundred pixels wide, so shaping more than this is
/// work nobody can see -- and `db` deliberately keeps whole values, which run to
/// megabytes for JSONB and PostGIS. The row inspector is where a whole value
/// gets read; this is the visual-only clip the spec's §4.4 allows.
const CELL_DISPLAY_LIMIT: usize = 300;

/// ponytail: the inspector shows the value, not a column's worth of it -- but
/// "the value" has to stop somewhere, because a multi-megabyte document laid
/// out as wrapped text stalls the frame it is laid out in. A double click on
/// the cell still copies all of it. Raise this if a real value gets cut.
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

pub struct ResultGrid {
    columns: Vec<Column>,
    result: QueryResult,
    /// Clipped, ref-counted copies built once per result set. `render_td` runs
    /// for every visible cell on every frame, so it must not allocate.
    display: Vec<Vec<Option<SharedString>>>,
    /// Which row each screen position shows. Sorting permutes this rather than
    /// the rows, so the server's order is always still there to return to, and
    /// nothing that holds a row index has to be told the rows moved.
    order: Vec<usize>,
    /// The last cell copied, marked so a copy is visible. A double click that
    /// leaves the screen unchanged reads as a click that did nothing.
    copied: Option<(usize, usize)>,
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
                    .sortable()
                    // The cell padding is Slate's, applied in `render_td` and
                    // `render_th`. Taking the library's as well indents every
                    // value twice and leaves a column's width unknowable here.
                    .p_0()
            })
            .collect();

        Self {
            columns,
            order: (0..display.len()).collect(),
            result,
            display,
            copied: None,
        }
    }

    /// The row a screen position is showing.
    fn source_row(&self, row_ix: usize) -> Option<usize> {
        self.order.get(row_ix).copied()
    }

    /// The whole value behind a cell, not the clipped one the grid paints: a
    /// column is a couple of hundred pixels wide and a JSONB document is not,
    /// and copying what happens to fit would be the same bug as reading a value
    /// through the column.
    fn cell(&self, source_row: usize, col_ix: usize) -> Option<&str> {
        self.result.rows.get(source_row)?.get(col_ix)?.as_deref()
    }

    /// Every column of one row, named, typed where the type is known, and
    /// carrying the value itself rather than the string the column had room
    /// for. This is what the row inspector reads.
    pub fn fields(&self, row_ix: usize) -> Vec<Field> {
        let Some(source_row) = self.source_row(row_ix) else {
            return Vec::new();
        };

        self.result
            .columns
            .iter()
            .enumerate()
            .map(|(col_ix, column)| Field {
                name: column.name.clone().into(),
                data_type: column.data_type.clone().map(SharedString::from),
                value: self
                    .cell(source_row, col_ix)
                    .map(|value| clip_to(value, FIELD_DISPLAY_LIMIT).into()),
            })
            .collect()
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
fn fitted_width(
    name: &str,
    display: &[Vec<Option<SharedString>>],
    col_ix: usize,
) -> gpui::Pixels {
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

/// Two cells ordered the way the values read rather than the way the text
/// sorts: every value arrives as text over the simple protocol, so a numeric
/// column would otherwise put 10 before 9, and a name column would put every
/// capital letter ahead of every lowercase one.
///
/// NULL sorts last in both directions. An absent value is not a small value,
/// and a descending sort that leads with a screen of NULLs has buried the
/// answer the sort was asked for.
fn compare(a: Option<&str>, b: Option<&str>, ascending: bool) -> Ordering {
    let (a, b) = match (a, b) {
        (Some(a), Some(b)) => (a, b),
        (None, None) => return Ordering::Equal,
        (None, Some(_)) => return Ordering::Greater,
        (Some(_), None) => return Ordering::Less,
    };

    let ordering = match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        (Ok(a), Ok(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
        _ => a
            .chars()
            .flat_map(char::to_lowercase)
            .cmp(b.chars().flat_map(char::to_lowercase)),
    };

    match ascending {
        true => ordering,
        false => ordering.reverse(),
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

    /// Sorting is Slate's, not the server's: re-running the statement with an
    /// `ORDER BY` would rewrite the user's SQL, and it would return a different
    /// snapshot of the table than the rows already on screen.
    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let mut order: Vec<usize> = (0..self.result.rows.len()).collect();

        if sort != ColumnSort::Default {
            let ascending = sort == ColumnSort::Ascending;
            // Stable, so equal values keep the order the server sent them in.
            order.sort_by(|&a, &b| {
                compare(self.cell(a, col_ix), self.cell(b, col_ix), ascending)
            });
        }

        self.order = order;
        cx.notify();
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let muted = theme(cx).text_muted;
        div()
            .h_full()
            // Not `size_full`: the sort control is the header cell's next
            // sibling, and a header that takes the whole width pushes it out
            // of a cell that clips.
            .flex_1()
            .min_w_0()
            .px(px(layout::SPACE_SM))
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(px(layout::TEXT_SM))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(muted)
            .child(self.columns[col_ix].name.clone())
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        // Rows are not guaranteed rectangular and an index can outlive the
        // result set it was taken from. Indexing here would abort the process
        // mid-paint and take the user's editor buffer with it.
        let source_row = self.source_row(row_ix);
        let cell = source_row
            .and_then(|source_row| self.display.get(source_row))
            .and_then(|row| row.get(col_ix))
            .and_then(Option::as_ref);
        let (text, faint, copied_bg) = {
            let t = theme(cx);
            (t.text, t.text_faint, t.element_active)
        };
        // Marked by the row it copied, not the position that row was in: a
        // sort moves the rows and the clipboard does not follow them.
        let copied = source_row.is_some() && self.copied == source_row.map(|row| (row, col_ix));

        div()
            .id(("cell", row_ix * self.columns.len() + col_ix))
            .size_full()
            .px(px(layout::SPACE_SM))
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_color(if cell.is_some() { text } else { faint })
            // Italic so a NULL cannot be mistaken for the four-letter string.
            .when(cell.is_none(), |cell| cell.italic())
            // Held until the next copy rather than timed out: this is a mark of
            // what is on the clipboard, and that does not expire either.
            .when(copied, |cell| cell.bg(copied_bg))
            .child(cell.cloned().unwrap_or(NULL_LABEL))
            // The whole value, past both what the column shows and what the
            // row inspector shows.
            .on_double_click(cx.listener(move |table, _, _, cx| {
                let Some(source_row) = table.delegate().source_row(row_ix) else {
                    return;
                };
                let Some(value) = table.delegate().cell(source_row, col_ix) else {
                    return;
                };
                cx.write_to_clipboard(ClipboardItem::new_string(value.to_string()));
                table.delegate_mut().copied = Some((source_row, col_ix));
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

    /// The rows in the order they would be painted in.
    fn shown(grid: &ResultGrid) -> Vec<Option<&str>> {
        (0..grid.order.len())
            .map(|row_ix| grid.cell(grid.source_row(row_ix).unwrap(), 0))
            .collect()
    }

    /// `perform_sort` needs a window and a context; the ordering it applies
    /// does not, so the tests drive that directly.
    fn sorted(grid: &ResultGrid, ascending: bool) -> Vec<Option<&str>> {
        let mut order: Vec<usize> = (0..grid.result.rows.len()).collect();
        order.sort_by(|&a, &b| compare(grid.cell(a, 0), grid.cell(b, 0), ascending));
        order.into_iter().map(|row| grid.cell(row, 0)).collect()
    }

    #[test]
    fn a_number_column_sorts_as_numbers_not_as_text() {
        // The whole point: every value arrives as text, so plain string order
        // would put 10 between 1 and 9.
        let grid = grid_of(&[Some("9"), Some("10"), Some("1"), Some("-2.5")]);

        assert_eq!(
            sorted(&grid, true),
            vec![Some("-2.5"), Some("1"), Some("9"), Some("10")]
        );
        assert_eq!(
            sorted(&grid, false),
            vec![Some("10"), Some("9"), Some("1"), Some("-2.5")]
        );
    }

    #[test]
    fn text_sorts_by_letter_rather_than_by_case() {
        let grid = grid_of(&[Some("banana"), Some("Apple"), Some("cherry")]);

        assert_eq!(
            sorted(&grid, true),
            vec![Some("Apple"), Some("banana"), Some("cherry")]
        );
    }

    #[test]
    fn nulls_sort_last_in_both_directions() {
        // An absent value is not a small value, and a descending sort that
        // opens with a screen of NULLs has hidden the answer.
        let grid = grid_of(&[None, Some("b"), None, Some("a")]);

        assert_eq!(sorted(&grid, true), vec![Some("a"), Some("b"), None, None]);
        assert_eq!(sorted(&grid, false), vec![Some("b"), Some("a"), None, None]);
    }

    #[test]
    fn a_fresh_grid_shows_the_server_order() {
        let grid = grid_of(&[Some("c"), Some("a"), Some("b")]);

        assert_eq!(shown(&grid), vec![Some("c"), Some("a"), Some("b")]);
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
        assert_eq!(fields[0].data_type.as_ref().map(SharedString::as_ref), Some("int4"));
        assert_eq!(fields[0].value.as_ref().map(SharedString::as_ref), Some("7"));
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
        assert!(row.get(0).is_some());
        assert!(row.get(1).is_none(), "row should be short, not padded");
    }
}
