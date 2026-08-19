use gpui::{
    App, ClipboardItem, Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    InteractiveElementExt,
    table::{Column, TableDelegate, TableState},
};

use crate::{
    db::QueryResult,
    theme::{layout, theme},
};

/// ponytail: the grid paints a fixed-width column, so shaping more than this is
/// work nobody can see -- and `db` deliberately keeps whole values, which run to
/// megabytes for JSONB and PostGIS. The scrollable value inspector (spec §4.4)
/// is where a whole value gets read; this is the visual-only clip §4.4 allows.
const CELL_DISPLAY_LIMIT: usize = 300;

const NULL_LABEL: SharedString = SharedString::new_static("NULL");

pub struct ResultGrid {
    columns: Vec<Column>,
    result: QueryResult,
    /// Clipped, ref-counted copies built once per result set. `render_td` runs
    /// for every visible cell on every frame, so it must not allocate.
    display: Vec<Vec<Option<SharedString>>>,
    /// The last cell copied, marked so a copy is visible. A double click that
    /// leaves the screen unchanged reads as a click that did nothing.
    copied: Option<(usize, usize)>,
}

impl ResultGrid {
    pub fn empty() -> Self {
        Self::new(QueryResult::default())
    }

    pub fn new(result: QueryResult) -> Self {
        let columns = result
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                Column::new(index.to_string(), column.name.clone())
                    .width(px(layout::GRID_COLUMN_WIDTH))
                    .resizable(true)
                    .movable(false)
            })
            .collect();
        let display = result
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.as_deref().map(|value| clip(value).into()))
                    .collect()
            })
            .collect();

        Self {
            columns,
            result,
            display,
            copied: None,
        }
    }

    /// The whole value behind a cell, not the clipped one the grid paints: a
    /// column is 180 pixels wide and a JSONB document is not, and copying what
    /// happens to fit would be the same bug as reading it through the column.
    fn value(&self, row_ix: usize, col_ix: usize) -> Option<&str> {
        self.result
            .rows
            .get(row_ix)?
            .get(col_ix)?
            .as_deref()
    }
}

/// Cut to a character count, never a byte count: slicing bytes panics in the
/// middle of a codepoint, and the values here are arbitrary user data.
fn clip(value: &str) -> String {
    match value.char_indices().nth(CELL_DISPLAY_LIMIT) {
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
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let muted = theme(cx).text_muted;
        div()
            .size_full()
            .px(px(layout::SPACE_SM))
            .flex()
            .items_center()
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
        let cell = self
            .display
            .get(row_ix)
            .and_then(|row| row.get(col_ix))
            .and_then(Option::as_ref);
        let (text, faint, copied_bg) = {
            let t = theme(cx);
            (t.text, t.text_faint, t.element_active)
        };
        let copied = self.copied == Some((row_ix, col_ix));

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
            // The whole value, which is the only way to get at one that is
            // wider than its column until the value inspector exists.
            .on_double_click(cx.listener(move |table, _, _, cx| {
                let Some(value) = table.delegate().value(row_ix, col_ix) else {
                    return;
                };
                cx.write_to_clipboard(ClipboardItem::new_string(value.to_string()));
                table.delegate_mut().copied = Some((row_ix, col_ix));
                cx.notify();
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Column as DbColumn;

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
            columns: vec![DbColumn { name: "a".into() }],
            rows: vec![vec![Some(value.clone())], vec![None]],
            ..QueryResult::default()
        });

        assert_eq!(grid.value(0, 0), Some(value.as_str()));
        assert_ne!(
            grid.display[0][0].as_ref().map(SharedString::as_ref),
            Some(value.as_str())
        );
        // A NULL is an absent value, not the string the cell paints for one.
        assert_eq!(grid.value(1, 0), None);
        assert_eq!(grid.value(9, 9), None);
    }

    #[test]
    fn a_short_row_reads_as_absent_rather_than_panicking() {
        // A ragged result set must not be able to abort the render pass.
        let grid = ResultGrid::new(QueryResult {
            columns: vec![
                DbColumn { name: "a".into() },
                DbColumn { name: "b".into() },
            ],
            rows: vec![vec![Some("only one cell".into())]],
            ..QueryResult::default()
        });

        let row = &grid.display[0];
        assert!(row.get(0).is_some());
        assert!(row.get(1).is_none(), "row should be short, not padded");
    }
}
