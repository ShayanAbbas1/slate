//! The main pane: what the tab in front is showing.
//!
//! Every function here was a `Workspace` associated function that never touched
//! `self` — a pure function of the profile it draws and the theme it reads. They
//! moved out whole; nothing changed but the indentation.
//!
//! `render_main_content` is the only way in. Everything else is a part of the
//! surface it assembles, which is why the rest of the module is private.

use gpui::{
    AnyElement, ClickEvent, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    Disableable, IconName, Sizable,
    input::{Input, InputState},
    resizable::{resizable_panel, v_resizable},
    spinner::Spinner,
    table::{Table, TableDelegate, TableState},
};

use crate::{
    CancelQuery, ClearFilter, CloseTarget, Control, EDITOR_FONT_SIZE_MAX, EDITOR_FONT_SIZE_MIN,
    NewQuery, NewRow, NextPage, ObjectBody, ObjectTab, PreviousPage, Profile, QueryState,
    ResetEditorZoom, RunQuery, SaveQuery, SetRowLimit, Settings, StructureState, Tab, Tone,
    Workspace, ZoomEditorIn, ZoomEditorOut, button, button_label, compact_count, db,
    db::RoutineKind,
    dialog, editor_zoom_percent,
    explorer::ROW_LIMITS,
    group_thousands, icon_button,
    icons::icon,
    key_hint, object_icon,
    palette::Mode as PaletteMode,
    result_grid,
    result_grid::ResultGrid,
    result_pane_is_expanded, row_icon, section_label,
    theme::{FontSlot, Theme, fonts, layout, theme},
};

pub fn render_main_content(
    profile: &Profile,
    editor_font_size: f32,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let body = match profile.session.active_object() {
        Some(tab) => render_object(tab, cx),
        None => render_query_surface(profile, editor_font_size, cx),
    };

    div()
        .size_full()
        .flex()
        .flex_col()
        // Chrome, so the strip reads as the frame the surfaces sit in --
        // and chrome is the frost, which is already painted beneath it.
        .child(render_tab_strip(profile, editor_font_size, cx))
        .child(div().flex_1().min_h_0().child(body))
        .into_any_element()
}

/// The editor over the rows it produces. Every runnable surface is this:
/// the query buffer and an opened relation differ in where their SQL came
/// from, not in what they are.
fn render_editor_surface(
    split: gpui::ElementId,
    editor: &Entity<InputState>,
    font_size: f32,
    query: &QueryState,
    bottom: AnyElement,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    let code = fonts(cx).editor.clone();

    // The editor is the prompt, one tone behind its results -- and one step
    // more transparent, since it is also one step further from the data.
    let top = div()
        .key_context("Editor")
        .size_full()
        .bg(t.panel_glass())
        .p(px(layout::SPACE_LG))
        .font_family(code)
        .child(
            Input::new(editor)
                .h_full()
                .appearance(false)
                .bordered(false)
                .focus_bordered(false)
                .text_size(px(font_size))
                .line_height(px(font_size * 1.55)),
        );

    let expanded = result_pane_is_expanded(query);
    let (editor_height, results_height) = if expanded {
        (
            layout::EDITOR_DEFAULT_HEIGHT,
            layout::RESULTS_DEFAULT_HEIGHT,
        )
    } else {
        (layout::EDITOR_EMPTY_HEIGHT, layout::RESULTS_EMPTY_HEIGHT)
    };

    v_resizable((split, if expanded { "expanded" } else { "compact" }))
        .child(
            resizable_panel()
                .size(px(editor_height))
                .size_range(px(layout::EDITOR_MIN_HEIGHT)..px(layout::EDITOR_MAX_HEIGHT))
                .child(top),
        )
        .child(
            resizable_panel()
                .size(px(results_height))
                .size_range(px(layout::RESULTS_MIN_HEIGHT)..gpui::Pixels::MAX)
                .child(bottom),
        )
        .into_any_element()
}

fn render_query_surface(
    profile: &Profile,
    editor_font_size: f32,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let Some(tab) = profile.session.active_query_tab() else {
        return div().into_any_element();
    };
    let bottom = render_results(&tab.query, &tab.results, true, cx);
    render_editor_surface(
        // Keyed by the buffer rather than the profile: two query tabs are two
        // splits, and sharing one id would carry the first one's drag position
        // onto the second.
        gpui::ElementId::from((
            gpui::ElementId::from("query-result-split"),
            gpui::SharedString::from(format!("{}-{}", profile.id, tab.id)),
        )),
        &tab.editor,
        editor_font_size,
        &tab.query,
        bottom,
        cx,
    )
}

/// An opened object. A relation's generated `SELECT` is an ordinary buffer
/// the user can edit and run; only a routine, which has nothing to run, is
/// read-only.
fn render_object(tab: &ObjectTab, cx: &mut Context<Workspace>) -> AnyElement {
    let t = *theme(cx);
    let ObjectBody::Relation {
        showing_structure,
        structure,
        results,
        query,
        filter,
        filter_input,
        ..
    } = &tab.body
    else {
        return render_routine(tab, cx);
    };

    if *showing_structure {
        return div()
            .size_full()
            .min_h_0()
            .bg(t.data_glass())
            .child(render_structure(structure, cx))
            .into_any_element();
    }

    div()
        .size_full()
        .flex()
        .flex_col()
        .child(render_filter_bar(filter, filter_input, t))
        .child(
            div()
                .flex_1()
                .min_h_0()
                .child(render_results(query, results, false, cx)),
        )
        .into_any_element()
}

/// The filter over a preview's rows, above the grid the pager sits over —
/// gated on the same one state, because a structure listing has no rows to
/// narrow.
fn render_filter_bar(filter: &str, input: &Entity<InputState>, t: Theme) -> AnyElement {
    div()
        .w_full()
        .h(px(layout::TAB_HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_XS))
        .pl(px(layout::SPACE_MD))
        .pr(px(layout::SPACE_SM))
        .child(row_icon(t, icon::SEARCH))
        .child(Input::new(input).small().min_w_0().flex_1())
        // Only once there is something to clear: a button that does nothing is
        // a control to read past.
        .children((!filter.is_empty()).then(|| {
            icon_button(
                "clear-filter",
                icon::CLOSE,
                Tone::Quiet,
                Control::Compact,
                t,
            )
            .tooltip("Clear filter")
            .on_click(move |_, window, cx| {
                window.dispatch_action(Box::new(ClearFilter), cx);
            })
        }))
        .into_any_element()
}

/// The "New row" form, over the preview it was opened on (spec §4).
///
/// The buttons are Cancel and **Review SQL**: this generates the statement and
/// shows it, and running it is the review panel's ask, not this one's.
pub fn render_new_row_form(
    workspace: &Workspace,
    cx: &mut Context<Workspace>,
) -> Option<AnyElement> {
    let t = *theme(cx);
    let profile = workspace.profile()?;
    let form = profile.session.insert_form.as_ref()?;
    if form.tab != profile.session.active {
        return None;
    }

    let fields: Vec<AnyElement> = form
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let null_workspace = cx.entity().downgrade();
            div()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_XS))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_SM))
                        .child(div().flex_1().min_w_0().child(field.column.clone()))
                        .child(
                            div()
                                .text_size(px(layout::TEXT_XS))
                                .text_color(t.text_faint)
                                .child(field.data_type.clone()),
                        )
                        .child(
                            button(
                                ("insert-null", index),
                                "NULL",
                                // Filled while it is on, because whether this
                                // field is a NULL is the only thing the chip
                                // has to say.
                                if field.nulled {
                                    Tone::Primary
                                } else {
                                    Tone::Quiet
                                },
                                Control::Inline,
                                t,
                            )
                            .on_click(move |_, _, cx| {
                                _ = null_workspace.update(cx, |workspace, cx| {
                                    workspace.toggle_insert_null(index, cx);
                                });
                            }),
                        ),
                )
                .child(Input::new(&field.input).small())
                .into_any_element()
        })
        .collect();

    let cancel_workspace = cx.entity().downgrade();
    let review_workspace = cancel_workspace.clone();

    Some(
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                dialog(t)
                    .child(section_label(t, "New row"))
                    // The one line that says what an empty field means, because
                    // the three-way rule is invisible otherwise.
                    .child(
                        div()
                            .text_size(px(layout::TEXT_SM))
                            .text_color(t.text_faint)
                            .child(
                                "A field left blank is left out, so the column keeps its default.",
                            ),
                    )
                    .child(
                        div()
                            .id("new-row-fields")
                            .max_h(px(320.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap(px(layout::SPACE_MD))
                            .children(fields),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(layout::SPACE_SM))
                            .child(
                                button(
                                    "cancel-new-row",
                                    "Cancel",
                                    Tone::Quiet,
                                    Control::Standard,
                                    t,
                                )
                                .on_click(move |_, _, cx| {
                                    _ = cancel_workspace.update(cx, |workspace, cx| {
                                        workspace.close_new_row(cx);
                                    });
                                }),
                            )
                            .child(
                                button(
                                    "review-new-row",
                                    "Review SQL",
                                    Tone::Primary,
                                    Control::Standard,
                                    t,
                                )
                                .on_click(move |_, _, cx| {
                                    _ = review_workspace.update(cx, |workspace, cx| {
                                        workspace.confirm_new_row(cx);
                                    });
                                }),
                            ),
                    ),
            )
            .into_any_element(),
    )
}

fn render_routine(tab: &ObjectTab, cx: &mut Context<Workspace>) -> AnyElement {
    let t = *theme(cx);
    let code = fonts(cx).editor.clone();
    let ObjectBody::Routine(routine) = &tab.body else {
        return div().into_any_element();
    };
    let kind = match routine.kind {
        RoutineKind::Function => "Function",
        RoutineKind::Procedure => "Procedure",
    };

    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(t.panel_glass())
        .child(
            div()
                .p(px(layout::SPACE_LG))
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_SM))
                .child(
                    div()
                        .text_size(px(layout::TEXT_LG))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("{}.{}", tab.schema, tab.name)),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(layout::SPACE_LG))
                        .text_size(px(layout::TEXT_SM))
                        .text_color(t.text_muted)
                        .child(kind)
                        .child(format!("Language: {}", routine.language))
                        .children(
                            (!routine.result_type.is_empty())
                                .then(|| div().child(format!("Returns: {}", routine.result_type))),
                        )
                        .child(div().ml_auto().child(key_hint(
                            t,
                            "escape",
                            "returns to the editor",
                        ))),
                ),
        )
        .child(
            div()
                .id("routine-definition")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p(px(layout::SPACE_LG))
                .font_family(code)
                .child(routine.definition.clone()),
        )
        .into_any_element()
}

/// The results plane: the brightest tone, because the data is the point.
///
/// A short status is centred and set in the app face -- it is a sentence
/// about the pane, not query output. An error keeps the editor's monospace
/// and the left edge, because it quotes the server and gets read against the
/// SQL above it. The grid and every message are alternatives, not layers: a
/// full-size message beside a full-size table gets pushed off the pane
/// entirely.
fn render_results(
    query: &QueryState,
    results: &Entity<TableState<ResultGrid>>,
    is_query: bool,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    let code = fonts(cx).editor.clone();
    let grid = fonts(cx).grid.clone();
    let centered = |child: AnyElement| {
        div()
            .size_full()
            .p(px(layout::SPACE_LG))
            .flex()
            .items_center()
            .justify_center()
            .child(child)
            .into_any_element()
    };
    let quiet_line = |line: String| {
        div()
            .text_size(px(layout::TEXT_SM))
            .text_color(t.text_muted)
            .child(line)
            .into_any_element()
    };
    // The default `Loader` icon names a file Slate's asset source does not
    // serve, so the spinner has to be pointed at the one it does.
    let spinner = || {
        Spinner::new()
            .icon(IconName::LoaderCircle)
            .color(t.text_muted.into())
            .small()
            .into_any_element()
    };

    let cancel = |cx: &mut Context<Workspace>| {
        // A word rather than an icon: a square or a cross beside a status line
        // reads as "close this", and the quiet tone is what keeps it from
        // competing with rows that are still coming.
        button("cancel-query", "Cancel", Tone::Quiet, Control::Compact, t).on_click(cx.listener(
            |workspace, _, window, cx| {
                workspace.cancel_query(&CancelQuery, window, cx);
            },
        ))
    };
    // A refresh keeps the rows it is replacing (`execute_and_then`'s
    // `keep_rows`), and a centred spinner over rows the user is still reading
    // hides the data this pane is for. So every state that has rows behind it
    // falls through to the grid, and the run says so in a strip above it
    // instead of in place of it.
    let has_rows = results.read(cx).delegate().rows_count(cx) > 0;

    let message = match query {
        QueryState::Idle if is_query => Some(centered(
            key_hint(
                t,
                "cmd-enter",
                "runs the selection or statement under the cursor",
            )
            .into_any_element(),
        )),
        // A preview runs the moment its tab is shown, so an idle one is a
        // tab that is about to run rather than one waiting to be asked. It has
        // nothing to cancel yet, though, which is the whole difference here.
        QueryState::Idle if !has_rows => Some(centered(spinner())),
        QueryState::Running if !has_rows => Some(centered(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(layout::SPACE_MD))
                .child(spinner())
                .child(cancel(cx))
                .into_any_element(),
        )),
        QueryState::Failed(error) => {
            let position = error
                .position
                .map(|position| format!(" (at byte {position})"))
                .unwrap_or_default();
            Some(
                div()
                    .size_full()
                    .p(px(layout::SPACE_LG))
                    .font_family(code)
                    .text_color(t.danger)
                    .child(format!("{}{position}", error.message))
                    .into_any_element(),
            )
        }
        // A restored snapshot written before it kept a row count is `Complete`
        // over zero rows it can nonetheless show, so the count alone cannot
        // decide this.
        QueryState::Complete {
            rows,
            rows_affected,
            ..
        } if *rows == 0 && !has_rows => Some(centered(quiet_line(match rows_affected {
            Some(rows) => format!("Query completed. Server row count: {rows}."),
            None => "Query completed.".into(),
        }))),
        _ => None,
    };

    // Values are read by comparing them down a column, which only lines up in
    // a monospaced face -- and the header inherits it, so the heading of a
    // column sits in the same rhythm as its values. The library's table sets
    // no family of its own, so this is where the cells and their headings get
    // theirs.
    let content = message.unwrap_or_else(|| {
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .children(matches!(query, QueryState::Running).then(|| {
                div()
                    .h(px(layout::TAB_HEIGHT))
                    .flex_shrink_0()
                    .px(px(layout::SPACE_SM))
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .border_b_1()
                    .border_color(t.border)
                    .child(spinner())
                    .child(quiet_line("Refreshing…".into()))
                    .child(div().ml_auto().child(cancel(cx)))
            }))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .min_h_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .font_family(grid)
                            // The grid's own delegate has no key hook and the
                            // focused element is the table root, so `enter` is
                            // caught here on its way out of the Table context.
                            .on_action(cx.listener(Workspace::edit_cell))
                            .on_action(cx.listener(Workspace::copy_cell))
                            .on_action(cx.listener(Workspace::set_null))
                            .on_action(cx.listener(Workspace::delete_row))
                            .on_action(cx.listener(Workspace::follow_foreign_key))
                            .child(Table::new(results).bordered(false).stripe(false)),
                    )
                    .children(render_row_inspector(results, cx)),
            )
            .into_any_element()
    });

    div()
        .size_full()
        .min_h_0()
        .bg(t.data_glass())
        .child(content)
        .into_any_element()
}

/// The selected row, one field per line, beside the grid.
///
/// A row read across a grid is a row read against the column headings
/// twenty columns away; read down a list it is just a row. The list also
/// has room for a value the column had to clip, which is what makes this
/// the value inspector the spec asks for in §4.4.
///
/// Nothing here is state of Slate's own: the selected row belongs to the
/// grid, so the panel cannot disagree with the highlight in the grid, and
/// arrow keys move both.
fn render_row_inspector(
    results: &Entity<TableState<ResultGrid>>,
    cx: &mut Context<Workspace>,
) -> Option<AnyElement> {
    let t = *theme(cx);
    let grid = fonts(cx).grid.clone();

    let (row_ix, rows, fields) = {
        let table = results.read(cx);
        let row_ix = table.selected_row()?;
        (
            row_ix,
            table.delegate().rows_count(cx),
            table.delegate().fields(row_ix),
        )
    };
    // A selection can outlive the rows it was made against.
    if fields.is_empty() {
        return None;
    }

    let table = results.clone();
    Some(
        div()
            .w(px(layout::INSPECTOR_WIDTH))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            // The same plane as the results, separated by its edge rather
            // than its tone: a second tint here reads as a slab pasted over
            // the window instead of a panel inside it.
            .border_l_1()
            .border_color(t.border)
            .child(
                div()
                    .h(px(layout::TAB_HEIGHT))
                    .px(px(layout::SPACE_SM))
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .child(
                        div()
                            .text_size(px(layout::TEXT_SM))
                            .text_color(t.text_muted)
                            .child(format!(
                                "Row {} of {}",
                                group_thousands(row_ix as u64 + 1),
                                group_thousands(rows as u64)
                            )),
                    )
                    .child(
                        div().ml_auto().child(
                            icon_button(
                                "close-row-inspector",
                                icon::CLOSE,
                                Tone::Quiet,
                                Control::Compact,
                                t,
                            )
                            .tooltip("Close the row panel")
                            .on_click(move |_, _, cx| {
                                table.update(cx, |table, cx| table.clear_selection(cx));
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .id("row-inspector")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(layout::SPACE_SM))
                    .pb(px(layout::SPACE_SM))
                    .flex()
                    .flex_col()
                    .gap(px(layout::SPACE_MD))
                    .children(fields.into_iter().map(|field| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(layout::SPACE_XS))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(layout::SPACE_SM))
                                    .child(
                                        div()
                                            .text_size(px(layout::TEXT_SM))
                                            .text_color(t.text_muted)
                                            .child(field.name),
                                    )
                                    // Absent rather than guessed: a type
                                    // Slate could not learn is not shown as
                                    // one it inferred from the text.
                                    .children(field.data_type.map(|data_type| {
                                        div()
                                            .ml_auto()
                                            .flex_shrink_0()
                                            .text_size(px(layout::TEXT_XS))
                                            .text_color(t.text_faint)
                                            .child(data_type)
                                    })),
                            )
                            .child(
                                div()
                                    .font_family(grid.clone())
                                    .text_size(px(layout::TEXT_SM))
                                    .map(|value| match field.value {
                                        Some(text) => value.text_color(t.text).child(text),
                                        // Italic so a NULL cannot be read
                                        // as the four-letter string.
                                        None => value
                                            .text_color(t.text_faint)
                                            .italic()
                                            .child(result_grid::NULL_LABEL),
                                    }),
                            )
                    })),
            )
            .into_any_element(),
    )
}

/// One segment of the Data | Structure pair. A quiet chip rather than a
/// filled button: it selects a view of the same object, it does not act.
fn preview_tab(
    label: &'static str,
    path: &'static str,
    selected: bool,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let t = *theme(cx);
    div()
        .id(label)
        .flex()
        .items_center()
        .gap(px(layout::SPACE_XS))
        .h(px(24.))
        .px(px(layout::SPACE_SM))
        .rounded(px(layout::RADIUS_CONTROL))
        .text_size(px(layout::TEXT_SM))
        .map(|tab| {
            if selected {
                tab.bg(t.element_active).text_color(t.text)
            } else {
                tab.text_color(t.text_muted)
                    .hover(|style| style.bg(t.element_hover))
            }
        })
        .child(
            icon(path)
                .size(px(12.))
                .text_color(if selected { t.text } else { t.text_faint }),
        )
        .child(label)
        .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
            workspace.show_structure(label == "Structure", cx);
        }))
}

/// One row-limit choice. A chip rather than a menu: four numbers fit, and a
/// number behind a popover is a number nobody checks.
fn row_limit_chip(rows: usize, selected: bool, cx: &mut Context<Workspace>) -> AnyElement {
    let t = *theme(cx);
    div()
        .id(("row-limit", rows))
        .flex()
        .items_center()
        .h(px(24.))
        .px(px(layout::SPACE_SM))
        .rounded(px(layout::RADIUS_CONTROL))
        .text_size(px(layout::TEXT_SM))
        .map(|chip| {
            if selected {
                chip.bg(t.element_active).text_color(t.text)
            } else {
                chip.text_color(t.text_muted)
                    .hover(|style| style.bg(t.element_hover))
            }
        })
        .child(compact_count(rows))
        .on_click(cx.listener(move |_, _, window, cx| {
            window.dispatch_action(Box::new(SetRowLimit { rows }), cx);
        }))
        .into_any_element()
}

fn render_structure(state: &StructureState, cx: &mut Context<Workspace>) -> AnyElement {
    let t = *theme(cx);
    let code = fonts(cx).editor.clone();

    let structure = match state {
        StructureState::Loading => {
            return div()
                .p(px(layout::SPACE_LG))
                .text_color(t.text_muted)
                .child("Loading structure…")
                .into_any_element();
        }
        StructureState::Failed(message) => {
            return div()
                .p(px(layout::SPACE_LG))
                .text_color(t.danger)
                .child(message.clone())
                .into_any_element();
        }
        StructureState::Loaded(structure) => structure,
    };

    let heading = |label: &'static str| {
        div()
            .pt(px(layout::SPACE_MD))
            .child(section_label(t, label))
    };
    let name_column = |name: String| {
        div()
            .w(px(220.))
            .min_w(px(220.))
            .font_weight(FontWeight::MEDIUM)
            .child(name)
    };
    let definitions = |definitions: &[db::NamedDefinition]| {
        definitions
            .iter()
            .map(|definition| {
                div()
                    .flex()
                    .gap(px(layout::SPACE_MD))
                    .child(name_column(definition.name.clone()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(t.text_muted)
                            .child(definition.definition.clone()),
                    )
            })
            .collect::<Vec<_>>()
    };

    div()
        .id("structure")
        .size_full()
        .overflow_y_scroll()
        .p(px(layout::SPACE_LG))
        .font_family(code)
        .flex()
        .flex_col()
        .gap(px(layout::SPACE_XS))
        .child(heading("Columns"))
        .children(structure.columns.iter().map(|column| {
            div()
                .flex()
                .gap(px(layout::SPACE_MD))
                .child(name_column(column.name.clone()))
                .child(
                    div()
                        .w(px(200.))
                        .min_w(px(200.))
                        // The same colour the editor gives a type name, so
                        // structure and SQL read as one vocabulary.
                        .text_color(t.syntax_type)
                        .child(column.data_type.clone()),
                )
                .child(
                    div()
                        .w(px(80.))
                        .min_w(px(80.))
                        .text_color(t.text_muted)
                        .child(if column.nullable {
                            "nullable"
                        } else {
                            "not null"
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(t.text_muted)
                        .child(column.default.clone().unwrap_or_default()),
                )
        }))
        .children((!structure.indexes.is_empty()).then(|| heading("Indexes")))
        .children(definitions(&structure.indexes))
        .children((!structure.constraints.is_empty()).then(|| heading("Constraints")))
        .children(definitions(&structure.constraints))
        .into_any_element()
}

/// The tab strip. It sits directly above the editor and starts where the
/// editor's text does, so a tab labels the surface under it rather than the
/// window: the active one is lifted to the editor's tone, the rest are names
/// that reveal a wash on hover. No boxes, no hairlines — tone carries the
/// state.
fn render_tab_strip(
    profile: &Profile,
    editor_font_size: f32,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    let workspace = cx.entity().downgrade();
    let session = &profile.session;
    let on_query_tab = matches!(session.active, Tab::Query(_));
    let runnable = session.editor(session.active).is_some();

    let chip = |active: bool| {
        div()
            .h(px(layout::TAB_CHIP_HEIGHT))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(layout::SPACE_XS))
            .rounded(px(layout::RADIUS_CONTROL))
            .map(|tab| {
                if active {
                    tab.bg(t.panel).text_color(t.text)
                } else {
                    tab.text_color(t.text_muted)
                        .hover(|style| style.bg(t.element_hover))
                }
            })
    };
    let name_label = |name: String| {
        div()
            .max_w(px(180.))
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .child(name)
    };

    // One chip per unsaved buffer, numbered in strip order. There used to be
    // exactly one, because there used to be exactly one editor.
    let unsaved_count = session
        .queries
        .iter()
        .filter(|tab| tab.open_query.is_none())
        .count();
    let mut tabs = session
        .queries
        .iter()
        .filter(|tab| tab.open_query.is_none())
        .enumerate()
        .map(|(index, tab)| {
            let id = tab.id;
            let group = format!("unsaved-query-tab-{id}");
            let open_workspace = workspace.clone();
            let close_workspace = workspace.clone();
            let label = match index {
                0 => "New Query".to_string(),
                _ => format!("New Query {}", index + 1),
            };
            chip(session.active == Tab::Query(id))
                .id(("unsaved-query-tab", id as usize))
                .group(group.clone())
                .pl(px(layout::SPACE_SM))
                // The last one has no × and keeps the symmetric padding: a
                // profile always has somewhere to write, so it has no closed
                // state to offer.
                .map(|chip| match unsaved_count > 1 {
                    true => chip.pr(px(layout::SPACE_XS)),
                    false => chip.pr(px(layout::SPACE_SM)),
                })
                // A pen, not a file: an unsaved buffer is a place to write, and
                // the distinction is what makes the saved tabs read as files.
                .child(row_icon(t, icon::SCRATCH_QUERY))
                .child(label)
                .when(unsaved_count > 1, |chip| {
                    chip.child(
                        div()
                            .opacity(0.)
                            .group_hover(group, |style| style.opacity(1.))
                            .child(
                                icon_button(
                                    ("close-unsaved-query", id as usize),
                                    icon::CLOSE,
                                    Tone::Quiet,
                                    Control::Inline,
                                    t,
                                )
                                .tooltip("Close tab")
                                .on_click(move |_, _, cx| {
                                    // Or the chip underneath activates the tab
                                    // this just closed, in the same click.
                                    cx.stop_propagation();
                                    _ = close_workspace.update(cx, |workspace, cx| {
                                        workspace.ask_before_close(CloseTarget::Buffer(id), cx);
                                    });
                                }),
                            ),
                    )
                })
                .on_click(move |_, window, cx| {
                    _ = open_workspace.update(cx, |workspace, cx| {
                        workspace.activate_tab(Tab::Query(id), window, cx);
                    });
                })
                .into_any_element()
        })
        .collect::<Vec<_>>();

    tabs.extend(
        session
            .saved_queries
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let open_name = name.clone();
                let delete_name = name.clone();
                let open_workspace = workspace.clone();
                let delete_workspace = workspace.clone();
                let pending = session.pending_delete.as_deref() == Some(name);
                let active = session
                    .tab_holding(name)
                    .is_some_and(|id| session.active == Tab::Query(id));
                chip(active)
                    .id(("saved-query", index))
                    .group(format!("query-tab-{index}"))
                    .pl(px(layout::SPACE_SM))
                    .pr(px(layout::SPACE_XS))
                    .child(row_icon(t, icon::SAVED_QUERY))
                    .child(name_label(name.clone()))
                    .child(
                        // Revealed by its own tab, so the strip reads as names
                        // rather than a row of delete buttons.
                        div()
                            .when(!pending, |delete| {
                                delete
                                    .opacity(0.)
                                    .group_hover(format!("query-tab-{index}"), |style| {
                                        style.opacity(1.)
                                    })
                            })
                            .child(
                                // Armed, it says the word and takes the danger
                                // fill: the icon alone asks, the red confirms.
                                icon_button(
                                    ("delete-query", index),
                                    icon::DELETE,
                                    if pending { Tone::Danger } else { Tone::Quiet },
                                    Control::Inline,
                                    t,
                                )
                                .when(pending, |armed| {
                                    armed.w_auto().px(px(layout::SPACE_XS)).child(button_label(
                                        "Delete?",
                                        Tone::Danger,
                                        Control::Inline,
                                        t,
                                    ))
                                })
                                .tooltip("Delete query")
                                .on_click(move |_, _, cx| {
                                    // Or the chip underneath opens the query in
                                    // the same click, and the confirmation this
                                    // arms is cleared before it can be seen.
                                    cx.stop_propagation();
                                    _ = delete_workspace.update(cx, |workspace, cx| {
                                        workspace.arm_delete_saved_query(delete_name.clone(), cx);
                                    });
                                }),
                            ),
                    )
                    .on_click(move |_, window, cx| {
                        _ = open_workspace.update(cx, |workspace, cx| {
                            workspace.open_saved_query(open_name.clone(), window, cx);
                        });
                    })
                    .into_any_element()
            }),
    );

    // Opened objects sit after the queries, in the order they were opened.
    // Closing one is not destructive, so it gets a plain × rather than the
    // saved queries' confirmed delete.
    tabs.extend(session.objects.iter().map(|object| {
        let id = object.id;
        let group = format!("object-tab-{id}");
        let open_workspace = workspace.clone();
        let close_workspace = workspace.clone();
        chip(session.active == Tab::Object(id))
            .id(("object-tab", id as usize))
            .group(group.clone())
            .pl(px(layout::SPACE_SM))
            .pr(px(layout::SPACE_XS))
            .child(row_icon(t, object_icon(object.kind)))
            .child(name_label(object.name.clone()))
            // One relation can have as many tabs as it has filters (spec §6.3),
            // so a strip that labelled them all `customers` would cost a click
            // each to tell apart. Bounded and ellipsized: a filter can be long.
            .children((!object.filter().is_empty()).then(|| {
                div()
                    .max_w(px(120.))
                    .px(px(layout::SPACE_XS))
                    .rounded(px(layout::RADIUS_CONTROL))
                    .bg(t.element_active)
                    .text_size(px(layout::TEXT_XS))
                    .text_color(t.text_muted)
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(object.filter().to_string())
            }))
            .child(
                div()
                    .opacity(0.)
                    .group_hover(group, |style| style.opacity(1.))
                    .child(
                        icon_button(
                            ("close-object", id as usize),
                            icon::CLOSE,
                            Tone::Quiet,
                            Control::Inline,
                            t,
                        )
                        .tooltip("Close tab")
                        .on_click(move |_, _, cx| {
                            // Or the chip underneath activates the tab
                            // this just closed, in the same click.
                            cx.stop_propagation();
                            _ = close_workspace.update(cx, |workspace, cx| {
                                workspace.ask_before_close(CloseTarget::Object(id), cx);
                            });
                        }),
                    ),
            )
            .on_click(move |_, window, cx| {
                _ = open_workspace.update(cx, |workspace, cx| {
                    workspace.activate_tab(Tab::Object(id), window, cx);
                });
            })
            .into_any_element()
    }));

    let confirm_workspace = workspace.clone();
    let naming_a_rename = on_query_tab && session.open_query().is_some();
    let naming = session.naming.then(|| {
        div()
            .w(px(240.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_XS))
            // The input and the button share one size so the pair sits on a
            // single centreline instead of jostling.
            .child(Input::new(&session.save_name).small().flex_1())
            .child(
                icon_button(
                    "confirm-save-query",
                    if naming_a_rename {
                        icon::RENAME
                    } else {
                        icon::SAVE
                    },
                    Tone::Primary,
                    Control::Compact,
                    t,
                )
                .tooltip(if naming_a_rename {
                    "Rename query"
                } else {
                    "Save query"
                })
                .on_click(move |_, window, cx| {
                    _ = confirm_workspace.update(cx, |workspace, cx| {
                        workspace.confirm_save(window, cx);
                    });
                }),
            )
    });

    // A relation's tab shows the two views of an object from the strip: a
    // header of its own would be a second bar saying what this one already
    // says.
    let structure_toggle = session.active_object().and_then(|tab| match &tab.body {
        ObjectBody::Relation {
            showing_structure, ..
        } => Some(
            div()
                .flex_shrink_0()
                .flex()
                .gap(px(layout::SPACE_XS))
                .child(preview_tab("Data", icon::TABLE, !showing_structure, cx))
                .child(preview_tab(
                    "Structure",
                    icon::STRUCTURE,
                    *showing_structure,
                    cx,
                )),
        ),
        ObjectBody::Routine(_) => None,
    });

    // What the preview asked the server for, and the only control over it.
    // Beside the Data | Structure pair because it belongs to the same view:
    // it is a property of these rows, not of the window.
    let preview = session.active_object().and_then(|tab| match &tab.body {
        ObjectBody::Relation {
            limit,
            offset,
            query,
            showing_structure: false,
            ..
        } => Some((
            *limit,
            *offset,
            // A full page may have another behind it; a short one is the
            // relation's end. The same gate `turn_page` holds, read here only
            // to decide whether the button is worth drawing.
            matches!(query, QueryState::Complete { rows, .. } if *rows >= *limit),
        )),
        _ => None,
    });
    let row_limit = preview.map(|(limit, _, _)| {
        let chips: Vec<_> = ROW_LIMITS
            .into_iter()
            .map(|rows| row_limit_chip(rows, rows == limit, cx))
            .collect();
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_XS))
            .child(
                div()
                    .text_size(px(layout::TEXT_SM))
                    .text_color(t.text_faint)
                    .child("Rows"),
            )
            .children(chips)
    });
    // The pager appears only once there is somewhere to go: a first page
    // shorter than its limit is the whole relation, and arrows over it are
    // controls that can do nothing.
    let pager = preview.and_then(|(limit, offset, full_page)| {
        (offset > 0 || full_page).then(|| {
            div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(layout::SPACE_XS))
                .children((offset > 0).then(|| {
                    icon_button(
                        "previous-page",
                        icon::CHEVRON_LEFT,
                        Tone::Quiet,
                        Control::Compact,
                        t,
                    )
                    .tooltip("Previous page")
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(Box::new(PreviousPage), cx);
                    })
                }))
                .child(
                    div()
                        .text_size(px(layout::TEXT_SM))
                        .text_color(t.text_faint)
                        // Offsets are multiples of the limit by construction --
                        // paging moves a page at a time and every other change
                        // resets to the first -- so the page number is exact.
                        .child(format!("Page {}", offset / limit + 1)),
                )
                .children(full_page.then(|| {
                    icon_button(
                        "next-page",
                        icon::CHEVRON_RIGHT,
                        Tone::Quiet,
                        Control::Compact,
                        t,
                    )
                    .tooltip("Next page")
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(Box::new(NextPage), cx);
                    })
                }))
        })
    });

    // Gated exactly as the pager is: a structure tab has no rows to add one to.
    let new_row = preview.map(|_| {
        div().flex_shrink_0().child(
            button("new-row", "New row", Tone::Quiet, Control::Compact, t).on_click(
                |_, window, cx| {
                    window.dispatch_action(Box::new(NewRow), cx);
                },
            ),
        )
    });

    let zoom = editor_zoom_percent(editor_font_size);
    let named = on_query_tab && session.open_query().is_some();
    let new_workspace = workspace.clone();
    let save_workspace = workspace.clone();
    let rename_workspace = workspace.clone();
    let run_workspace = workspace.clone();

    div()
        .h(px(layout::TAB_HEIGHT))
        .w_full()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        // Starts where the editor's text does, so a tab lines up with the
        // buffer it names.
        .pl(px(layout::SPACE_LG))
        .pr(px(layout::SPACE_SM))
        .text_size(px(layout::TEXT_SM))
        .child(
            div()
                .id("query-tabs-scroll")
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(layout::SPACE_XS))
                .overflow_x_scroll()
                .children(tabs)
                .child(
                    // Beside the last tab, where a browser puts it, rather
                    // than orphaned at the far edge of the window.
                    icon_button(
                        "new-query-tab",
                        icon::PLUS,
                        Tone::Quiet,
                        Control::Compact,
                        t,
                    )
                    .tooltip_with_action("New query", &NewQuery, None)
                    .on_click(move |_, window, cx| {
                        _ = new_workspace.update(cx, |workspace, cx| {
                            workspace.new_query(&NewQuery, window, cx);
                        });
                    }),
                ),
        )
        .children(structure_toggle)
        .children(row_limit)
        .children(pager)
        .children(new_row)
        // 100% is not information; the readout appears only once the zoom
        // has somewhere to return to.
        .children((runnable && zoom != 100).then(|| {
            div()
                .flex_shrink_0()
                .text_color(t.text_faint)
                .child(format!("{zoom}% · ⌘0 resets"))
        }))
        .children(naming)
        // A named query is already written to disk on every swap, so there
        // is nothing for a save button to do that has not been done. What
        // it can still do is change the name.
        .children((runnable && !session.naming && named).then(|| {
            icon_button(
                "rename-query",
                icon::RENAME,
                Tone::Quiet,
                Control::Compact,
                t,
            )
            .tooltip("Rename query")
            .on_click(move |_, window, cx| {
                _ = rename_workspace.update(cx, |workspace, cx| {
                    workspace.rename_query(window, cx);
                });
            })
        }))
        .children((runnable && !session.naming && !named).then(|| {
            icon_button("save-query", icon::SAVE, Tone::Quiet, Control::Compact, t)
                .tooltip_with_action("Save query", &SaveQuery, None)
                .on_click(move |_, window, cx| {
                    _ = save_workspace.update(cx, |workspace, cx| {
                        workspace.save_query(&SaveQuery, window, cx);
                    });
                })
        }))
        .children(runnable.then(|| {
            // Filled where its neighbours are ghosts: running the buffer is
            // what the surface is for, and the fill is the only hierarchy
            // available without spending a colour on it.
            icon_button("run-query", icon::RUN, Tone::Primary, Control::Compact, t)
                .tooltip_with_action("Run", &RunQuery, None)
                .on_click(move |_, window, cx| {
                    _ = run_workspace.update(cx, |workspace, cx| {
                        workspace.run_query(&RunQuery, window, cx);
                    });
                })
        }))
        .into_any_element()
}

/// The app-wide settings, on the card every other modal is drawn on.
///
/// There is no Cancel and no OK. Every control here calls the same method the
/// keystroke or the palette row calls, and each of those has already written
/// the change to `profiles.toml` by the time this repaints — so Cancel would
/// have to undo a file, and Done only takes the card away.
pub fn render_settings(settings: &Settings, cx: &mut Context<Workspace>) -> AnyElement {
    let t = *theme(cx);
    let families = fonts(cx).clone();
    let font_size = settings.editor_font_size;
    let preview_rows = settings.preview_rows;

    let themes: Vec<AnyElement> = Theme::all()
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| {
            settings_chip(
                ("theme", index),
                candidate.name,
                candidate.name == t.name,
                cx,
                move |workspace, window, cx| workspace.set_theme(candidate, window, cx),
            )
        })
        .collect();

    // Disabled at the ends rather than clamped again here: `adjust_editor_zoom`
    // already refuses to go past them, and a button that looks live and does
    // nothing is worse than one that says it cannot.
    let zoom = div()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        .child(
            button("zoom-out", "−", Tone::Quiet, Control::Compact, t)
                .disabled(font_size <= EDITOR_FONT_SIZE_MIN)
                .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                    workspace.zoom_editor_out(&ZoomEditorOut, window, cx);
                })),
        )
        .child(
            div()
                .min_w(px(40.))
                .text_size(px(layout::TEXT_SM))
                .child(format!("{}%", editor_zoom_percent(font_size))),
        )
        .child(
            button("zoom-in", "+", Tone::Quiet, Control::Compact, t)
                .disabled(font_size >= EDITOR_FONT_SIZE_MAX)
                .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                    workspace.zoom_editor_in(&ZoomEditorIn, window, cx);
                })),
        )
        .child(
            button("zoom-reset", "Reset", Tone::Quiet, Control::Compact, t).on_click(cx.listener(
                |workspace, _: &ClickEvent, window, cx| {
                    workspace.reset_editor_zoom(&ResetEditorZoom, window, cx);
                },
            )),
        );

    // The palette rather than a dropdown of our own: it already lists every
    // family the text system resolved and marks the one in use. It opens over
    // this card and leaves it standing, so a pick lands back here.
    let font_rows: Vec<AnyElement> = [
        ("Chrome", FontSlot::Chrome),
        ("Editor", FontSlot::Editor),
        ("Grid", FontSlot::Grid),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (label, slot))| {
        let family = families.family(slot).clone();
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(layout::SPACE_MD))
            .child(
                div()
                    .text_size(px(layout::TEXT_SM))
                    .text_color(t.text_muted)
                    .child(label),
            )
            .child(settings_chip(
                ("font", index),
                family,
                true,
                cx,
                move |workspace, window, cx| {
                    workspace.open_palette(PaletteMode::Font(slot), window, cx);
                },
            ))
            .into_any_element()
    })
    .collect();

    let limits: Vec<AnyElement> = ROW_LIMITS
        .into_iter()
        .map(|rows| {
            settings_chip(
                ("preview-rows", rows),
                compact_count(rows),
                rows == preview_rows,
                cx,
                move |workspace, _, cx| workspace.set_preview_rows(rows, cx),
            )
        })
        .collect();

    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            dialog(t)
                .child(section_label(t, "Settings"))
                .child(settings_section(
                    t,
                    "Theme",
                    div().flex().gap(px(layout::SPACE_XS)).children(themes),
                ))
                .child(settings_section(t, "Editor zoom", zoom))
                .child(settings_section(
                    t,
                    "Fonts",
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(layout::SPACE_XS))
                        .children(font_rows),
                ))
                .child(settings_section(
                    t,
                    "Default limit",
                    div().flex().gap(px(layout::SPACE_XS)).children(limits),
                ))
                .child(div().flex().justify_end().child(
                    button("settings-done", "Done", Tone::Primary, Control::Standard, t).on_click(
                        cx.listener(|workspace, _: &ClickEvent, _, cx| {
                            workspace.close_settings(cx);
                        }),
                    ),
                )),
        )
        .into_any_element()
}

/// One setting: its label over whatever sets it.
fn settings_section(t: Theme, label: &str, controls: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(layout::SPACE_XS))
        .child(section_label(t, label))
        .child(controls)
}

/// One choice in the settings modal, in the row-limit chips' clothes: a handful
/// of values, all of them on screen, the one in force filled in.
fn settings_chip(
    id: impl Into<gpui::ElementId>,
    label: impl Into<gpui::SharedString>,
    selected: bool,
    cx: &mut Context<Workspace>,
    apply: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> AnyElement {
    let t = *theme(cx);
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(24.))
        .px(px(layout::SPACE_SM))
        .rounded(px(layout::RADIUS_CONTROL))
        .text_size(px(layout::TEXT_SM))
        .whitespace_nowrap()
        .map(|chip| {
            if selected {
                chip.bg(t.element_active).text_color(t.text)
            } else {
                chip.text_color(t.text_muted)
                    .hover(|style| style.bg(t.element_hover))
            }
        })
        .child(label.into())
        .on_click(cx.listener(move |workspace, _: &ClickEvent, window, cx| {
            apply(workspace, window, cx);
        }))
        .into_any_element()
}
