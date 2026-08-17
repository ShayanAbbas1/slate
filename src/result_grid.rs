use gpui::{App, Context, IntoElement, ParentElement, Styled, Window, div, px};
use gpui_component::table::{Column, TableDelegate, TableState};

use crate::{
    db::QueryResult,
    theme::{layout, theme},
};

pub struct ResultGrid {
    columns: Vec<Column>,
    result: QueryResult,
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
                    .movable(false)
            })
            .collect();
        Self { columns, result }
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
        let t = *theme(cx);
        div()
            .size_full()
            .px(px(layout::SPACE_SM))
            .flex()
            .items_center()
            .text_color(t.text_muted)
            .child(self.columns[col_ix].name.clone())
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let t = *theme(cx);
        let cell = self.result.rows[row_ix][col_ix].as_deref();
        div()
            .size_full()
            .px(px(layout::SPACE_SM))
            .flex()
            .items_center()
            .overflow_hidden()
            .text_color(if cell.is_some() { t.text } else { t.text_faint })
            .child(cell.unwrap_or("NULL").to_string())
    }
}
