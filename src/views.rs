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
    ParentElement, StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    IconName, InteractiveElementExt, Sizable,
    input::{Input, InputState},
    resizable::{resizable_panel, v_resizable},
    spinner::Spinner,
    table::{Table, TableDelegate, TableState},
};

use crate::{
    CancelQuery, Control, NewQuery, ObjectBody, ObjectTab, Profile, QueryState, RunQuery,
    SaveQuery, SetRowLimit, StructureState, Tab, Tone, Workspace, button, button_label,
    compact_count, db,
    db::RoutineKind,
    editor_zoom_percent,
    explorer::ROW_LIMITS,
    group_thousands, icon_button,
    icons::icon,
    key_hint, object_icon, result_grid,
    result_grid::ResultGrid,
    result_pane_is_expanded, row_icon, section_label,
    theme::{layout, theme},
};

pub fn render_main_content(profile: &Profile, cx: &mut Context<Workspace>) -> AnyElement {
    let body = match profile.session.active_object() {
        Some(tab) => render_object(tab, cx),
        None => render_query_surface(profile, cx),
    };

    div()
        .size_full()
        .flex()
        .flex_col()
        // Chrome, so the strip reads as the frame the surfaces sit in --
        // and chrome is the frost, which is already painted beneath it.
        .child(render_tab_strip(profile, cx))
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
    let mono = gpui_component::Theme::global(cx).mono_font_family.clone();

    // The editor is the prompt, one tone behind its results -- and one step
    // more transparent, since it is also one step further from the data.
    let top = div()
        .size_full()
        .bg(t.panel_glass())
        .p(px(layout::SPACE_LG))
        .font_family(mono)
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

fn render_query_surface(profile: &Profile, cx: &mut Context<Workspace>) -> AnyElement {
    let bottom = render_results(&profile.session.query, &profile.session.results, true, cx);
    render_editor_surface(
        gpui::ElementId::from((
            gpui::ElementId::from("query-result-split"),
            profile.id.clone(),
        )),
        &profile.session.editor,
        profile.session.editor_font_size,
        &profile.session.query,
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

    render_results(query, results, false, cx)
}

fn render_routine(tab: &ObjectTab, cx: &mut Context<Workspace>) -> AnyElement {
    let t = *theme(cx);
    let mono = gpui_component::Theme::global(cx).mono_font_family.clone();
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
                .font_family(mono)
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
    let mono = gpui_component::Theme::global(cx).mono_font_family.clone();
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

    let content = match query {
        QueryState::Idle if is_query => centered(
            key_hint(
                t,
                "cmd-enter",
                "runs the selection or statement under the cursor",
            )
            .into_any_element(),
        ),
        // A preview runs the moment its tab is shown, so an idle one is a
        // tab that is about to run rather than one waiting to be asked. It has
        // nothing to cancel yet, though, which is the whole difference here.
        QueryState::Idle => centered(spinner()),
        QueryState::Running => centered(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(layout::SPACE_MD))
                .child(spinner())
                // A word rather than an icon: a square or a cross beside a
                // status line reads as "close this", and the quiet tone is what
                // keeps it from competing with rows that are still coming.
                .child(
                    button("cancel-query", "Cancel", Tone::Quiet, Control::Compact, t).on_click(
                        cx.listener(|workspace, _, window, cx| {
                            workspace.cancel_query(&CancelQuery, window, cx);
                        }),
                    ),
                )
                .into_any_element(),
        ),
        QueryState::Failed(error) => {
            let position = error
                .position
                .map(|position| format!(" (at byte {position})"))
                .unwrap_or_default();
            div()
                .size_full()
                .p(px(layout::SPACE_LG))
                .font_family(mono)
                .text_color(t.danger)
                .child(format!("{}{position}", error.message))
                .into_any_element()
        }
        QueryState::Complete {
            rows,
            rows_affected,
            ..
        } if *rows == 0 => centered(quiet_line(match rows_affected {
            Some(rows) => format!("Query completed. Server row count: {rows}."),
            None => "Query completed.".into(),
        })),
        // Values are read by comparing them down a column, which only lines
        // up in a monospaced face -- and the header inherits it, so the
        // heading of a column sits in the same rhythm as its values.
        _ => div()
            .size_full()
            .flex()
            .min_h_0()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .font_family(mono)
                    // The grid's own delegate has no key hook and the focused
                    // element is the table root, so `enter` is caught here on
                    // its way out of the Table context.
                    .on_action(cx.listener(Workspace::edit_cell))
                    .on_action(cx.listener(Workspace::copy_cell))
                    .child(Table::new(results).bordered(false).stripe(false)),
            )
            .children(render_row_inspector(results, cx))
            .into_any_element(),
    };

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
    let mono = gpui_component::Theme::global(cx).mono_font_family.clone();

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
                                    .font_family(mono.clone())
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
    let mono = gpui_component::Theme::global(cx).mono_font_family.clone();

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
        .font_family(mono)
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
fn render_tab_strip(profile: &Profile, cx: &mut Context<Workspace>) -> AnyElement {
    let t = *theme(cx);
    let workspace = cx.entity().downgrade();
    let session = &profile.session;
    let on_query_tab = matches!(session.active, Tab::Query);
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
    let name_label = |name: String, transient: bool| {
        div()
            .max_w(px(180.))
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            // Italic for a tab nobody has asked to keep, the way every
            // editor marks one.
            .when(transient, |label| label.italic())
            .child(name)
    };

    let scratch_workspace = workspace.clone();
    let mut tabs = vec![
        chip(on_query_tab && session.open_query.is_none())
            .id("scratch-query-tab")
            .px(px(layout::SPACE_SM))
            // A pen, not a file: the scratch buffer is a place to write, and
            // the distinction is what makes the saved tabs read as files.
            .child(row_icon(t, icon::SCRATCH_QUERY))
            .child("New Query")
            .on_click(move |_, window, cx| {
                _ = scratch_workspace.update(cx, |workspace, cx| {
                    workspace.open_scratch_query(window, cx);
                });
            })
            .into_any_element(),
    ];

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
                let active = on_query_tab && session.open_query.as_deref() == Some(name);
                chip(active)
                    .id(("saved-query", index))
                    .group(format!("query-tab-{index}"))
                    .pl(px(layout::SPACE_SM))
                    .pr(px(layout::SPACE_XS))
                    .child(row_icon(t, icon::SAVED_QUERY))
                    .child(name_label(name.clone(), false))
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
                                .on_click(move |_, window, cx| {
                                    // Or the chip underneath opens the query in
                                    // the same click, and the confirmation this
                                    // arms is cleared before it can be seen.
                                    cx.stop_propagation();
                                    _ = delete_workspace.update(cx, |workspace, cx| {
                                        workspace.arm_delete_saved_query(
                                            delete_name.clone(),
                                            window,
                                            cx,
                                        );
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
        let keep_workspace = workspace.clone();
        let close_workspace = workspace.clone();
        chip(session.active == Tab::Object(id))
            .id(("object-tab", id as usize))
            .group(group.clone())
            .pl(px(layout::SPACE_SM))
            .pr(px(layout::SPACE_XS))
            .child(row_icon(t, object_icon(object.kind)))
            .child(name_label(object.name.clone(), object.transient))
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
                                workspace.close_object(id, cx);
                            });
                        }),
                    ),
            )
            .on_click(move |_, _, cx| {
                _ = open_workspace.update(cx, |workspace, cx| {
                    workspace.activate_tab(Tab::Object(id), cx);
                });
            })
            // The other half of the preview gesture: a double click on the
            // tab keeps it, exactly as it does in the tree.
            .on_double_click(move |_, _, cx| {
                _ = keep_workspace.update(cx, |workspace, cx| {
                    workspace.keep_object(id, cx);
                });
            })
            .into_any_element()
    }));

    let confirm_workspace = workspace.clone();
    let naming_a_rename = on_query_tab && session.open_query.is_some();
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
    let showing_rows = session.active_object().and_then(|tab| match &tab.body {
        ObjectBody::Relation {
            limit,
            showing_structure: false,
            ..
        } => Some(*limit),
        _ => None,
    });
    let row_limit = showing_rows.map(|limit| {
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

    let zoom = editor_zoom_percent(session.editor_font_size);
    let named = on_query_tab && session.open_query.is_some();
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
