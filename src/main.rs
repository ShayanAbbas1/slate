mod actions;
mod completion;
mod connection_form;
mod db;
mod explorer;
mod export;
mod filter;
mod palette;
mod result_grid;
mod session;
mod sql;
mod store;
mod views;
mod workspace;

mod icons;
mod theme;
mod tls;
mod ui;

use std::{borrow::Cow, collections::HashMap, path::PathBuf, rc::Rc, sync::Arc};

use gpui::{
    AnyElement, App, AppContext, Application, ClickEvent, ClipboardItem, Context, Entity,
    EntityInputHandler, FocusHandle, Focusable, FontWeight, InteractiveElement, IntoElement,
    KeyBinding, Menu, MenuItem, ParentElement, Render, StatefulInteractiveElement, Styled,
    TitlebarOptions, Window, WindowOptions, deferred, div, point, prelude::FluentBuilder, px,
};
use gpui_component::{
    Disableable, IndexPath, Root,
    input::{CompletionProvider, Enter, IndentInline, Input, InputEvent, InputState, Position},
    list::{List, ListEvent, ListItem, ListState},
    resizable::{h_resizable, resizable_panel},
    table::{TableEvent, TableState},
    tree::tree as render_tree,
};

use actions::{
    AcceptCompletion, AddFilter, ApplyEdits, CancelQuery, ClearFilter, CloseTab, CommandPalette,
    CopyCell, CycleTheme, DeleteRow, DiscardEdits, EditCell, FollowForeignKey, FuzzyOpen,
    NewConnection, NewQuery, NewRow, NextPage, NextProfile, NextTab, OpenSettings, PaletteNext,
    PalettePrevious, PreviousPage, PreviousProfile, PreviousTab, Quit, RemoveFilter,
    ResetEditorZoom, RunQuery, SaveQuery, SetFilterColumn, SetFilterOperator, SetFilterRaw,
    SetNull, SetRowLimit, ShowEditor, SortColumn, ToggleFilterJoin, ToggleNextJoin, ToggleSidebar,
    ZoomEditorIn, ZoomEditorOut,
};
use completion::SchemaCompletions;
use connection_form::{ConnectionForm, default_profile_name};
use db::{
    Catalog, Connection, ConnectionConfig, DbError, Engine, RelationKind, ServerConfig, SslMode,
};
use explorer::{ExplorerTarget, ObjectKind, PREVIEW_ROW_LIMIT, tree as build_explorer_tree};
use export::Format;
use filter::{
    Conjunction, FilterBar, FilterRow, Operator, changed_filter, cycle, derived_filter,
    filter_bars, filter_row, foreign_key_filter, relation_sql, restored_filter, sort_columns,
    sort_expression, value_placeholder,
};
use icons::{Icons, icon};
use palette::{Command, Mode as PaletteMode, Palette};
use result_grid::{PendingRow, ResultGrid};
use session::{
    ApplyReview, CatalogState, CloseTarget, Focus, InsertField, InsertForm, ObjectBody, ObjectTab,
    OpenedObject, Profile, ProfileState, QueryState, QueryTab, Refresh, Session, StructureState,
    Tab, close_target, insert_value, matching_tab, relation_kind, restored_state, show_snapshot,
};
use sql::{Buffer, SortKey};
use theme::{ConnectionColor, FontSlot, Fonts, Theme, fonts, layout, theme};
use ui::{
    Control, Tone, button, button_label, dialog, group_thousands, human_bytes, icon_button,
    object_icon, relative_age, row_icon, row_icon_tinted, row_readout, section_label, titlebar,
};
use workspace::{Settings, Workspace};

const EDITOR_FONT_SIZE_DEFAULT: f32 = 14.0;
const EDITOR_FONT_SIZE_MIN: f32 = 11.0;
const EDITOR_FONT_SIZE_MAX: f32 = 24.0;
const EDITOR_FONT_SIZE_STEP: f32 = 1.0;

/// The platform's window buttons, which Slate positions but does not draw.
const TRAFFIC_LIGHT_DIAMETER: f32 = 14.0;

fn result_grid(window: &mut Window, cx: &mut Context<Workspace>) -> Entity<TableState<ResultGrid>> {
    let grid = cx.new(|cx| {
        TableState::new(ResultGrid::empty(), window, cx)
            // Sorting is the grid's own, over the rows it already holds. It
            // never re-runs the statement, so the rows on screen stay the one
            // snapshot the server sent.
            .sortable(true)
            .col_movable(false)
            .col_resizable(true)
            .row_selectable(true)
            .col_selectable(true)
    });

    // The library's arrow keys move its own selection, which is a row or a
    // column and never a cell. Folded into the active cell here, they move the
    // ring instead -- so every grid is navigable by keyboard, and Slate needs
    // no arrow binding competing with the library's own actions.
    //
    // Hooked in the constructor because every relation tab builds its grid
    // through it: a subscription set up at one call site would leave the other
    // grid navigating an invisible selection.
    cx.subscribe(&grid, |_, table, event: &TableEvent, cx| match event {
        TableEvent::SelectRow(row) => {
            let row = *row;
            table.update(cx, |table, cx| {
                table.delegate_mut().select_row(row);
                cx.notify();
            });
        }
        TableEvent::SelectColumn(col) => {
            let col = *col;
            table.update(cx, |table, cx| {
                table.delegate_mut().select_col(col);
                cx.notify();
            });
        }
        // The library resizes its own copy of the columns, so a drag is only
        // in the delegate -- the thing a snapshot is taken from -- if it is
        // written back here.
        TableEvent::ColumnWidthsChanged(widths) => {
            let widths = widths.clone();
            table.update(cx, |table, _| table.delegate_mut().set_widths(&widths));
        }
        _ => {}
    })
    .detach();

    grid
}

/// Every pending row as one `UPDATE`, joined into a single string.
///
/// All-or-nothing, which each engine reaches differently, and
/// `Engine::transaction_start` is where that per-engine answer lives. Postgres
/// runs one submission as a single implicit transaction and needs nothing;
/// MySQL and SQLite commit every statement on its own, so a batch of more than
/// one is bracketed — in the statement text itself, where the user can read,
/// edit and undo it, because Slate does not open a transaction behind anyone's
/// back.
///
/// `None` when there is nothing to apply, and `None` — rather than a shorter
/// batch — when any one row cannot be written: a partial apply is not the change
/// the user made, and Slate would have no way to say which part of it ran.
fn update_batch(engine: Engine, rows: &[PendingRow]) -> Option<String> {
    if rows.is_empty() {
        return None;
    }

    fn borrowed(pairs: &[(String, String)]) -> Vec<(&str, &str)> {
        pairs
            .iter()
            .map(|(column, value)| (column.as_str(), value.as_str()))
            .collect()
    }
    fn borrowed_sets(pairs: &[(String, Option<String>)]) -> Vec<(&str, Option<&str>)> {
        pairs
            .iter()
            .map(|(column, value)| (column.as_str(), value.as_deref()))
            .collect()
    }
    let statements: Option<Vec<String>> = rows
        .iter()
        .map(|row| {
            sql::update_row(
                engine,
                &row.schema,
                &row.table,
                &borrowed_sets(&row.sets),
                &borrowed(&row.keys),
            )
            // Terminated, not separated: the last statement carries its
            // semicolon too, so appending to a buffer cannot fuse it onto
            // whatever the user writes next.
            .map(|statement| format!("{statement};"))
        })
        .collect();

    let batch = statements?.join("\n");
    let bracket = engine.transaction_start().filter(|_| rows.len() > 1);
    Some(match bracket {
        Some(start) => format!("{start};\n{batch}\nCOMMIT;"),
        None => batch,
    })
}

/// A statement at the front of the history, and there only once however many
/// times it has been run: a query run five times is one row to recall, not five
/// rows to read past.
fn remember_statement(history: &mut Vec<String>, sql: &str) {
    history.retain(|past| past != sql);
    history.insert(0, sql.to_string());
    history.truncate(store::HISTORY_DEPTH);
}

/// Slate's statement appended to the buffer the user is writing in.
///
/// The terminator is the whole subtlety: an unterminated statement with an
/// `UPDATE` appended to it becomes one statement, and the next `cmd+enter`
/// would send both as one. Slate is writing here because the user asked it to,
/// so the boundary of what they wrote has to survive the ask.
fn appended_statement(buffer: &str, statement: &str) -> String {
    let text = buffer.trim_end();
    if text.is_empty() {
        return statement.to_string();
    }

    let terminator = match text.ends_with(';') {
        true => "",
        false => ";",
    };
    format!("{text}{terminator}\n\n{statement}")
}

/// Write every one of a profile's buffers back to whichever file it came from.
///
/// All of them rather than the one in front: a buffer that is not visible is
/// still someone's unsaved work, and a tab switch is no longer the moment it
/// gets written. The first failure is the one reported and the rest are still
/// attempted — a full disk must not cost more buffers than it has to.
fn write_buffer(profile: &Profile, cx: &App) -> Result<(), String> {
    let mut failure = None;
    for tab in &profile.session.queries {
        let sql = tab.editor.read(cx).value().to_string();
        let written = match &tab.open_query {
            Some(name) => store::write_query(&profile.id, name, &sql),
            None => store::write_scratch(&profile.id, tab.id, &sql),
        };
        if let Err(message) = written {
            failure = failure.or(Some(message));
        }
    }
    match failure {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

/// Snapshot every tab's grid, so reopening a profile shows the rows it was
/// showing rather than an empty grid waiting on a re-run.
///
/// Only a `Complete` tab is written: an empty or failed grid is not a result,
/// and writing one would replace a good snapshot with nothing. Failures are
/// dropped rather than reported, unlike the buffers this runs beside -- a cache
/// that did not land costs a re-run, not somebody's unsaved work.
fn write_grids(profile: &Profile, cx: &App) {
    for tab in &profile.session.queries {
        if !matches!(tab.query, QueryState::Complete { .. }) {
            continue;
        }
        let grid = tab.results.read(cx).delegate().stored();
        // A statement that returned no columns produced no grid to keep --
        // which is every `UPDATE` and `DELETE` the buffer has run.
        if grid.columns.is_empty() {
            continue;
        }
        let _ = store::write_grid(
            &profile.id,
            &store::query_grid_key(tab.id),
            &store::StoredGrid {
                last_query: tab.last_query.clone(),
                ..grid
            },
        );
    }

    for tab in &profile.session.objects {
        let ObjectBody::Relation {
            results,
            query,
            sort,
            filter,
            limit,
            showing_structure,
            ..
        } = &tab.body
        else {
            continue;
        };
        if !matches!(query, QueryState::Complete { .. }) {
            continue;
        }
        let grid = results.read(cx).delegate().stored();
        if grid.columns.is_empty() {
            continue;
        }
        let _ = store::write_grid(
            &profile.id,
            &store::object_grid_key(&tab.schema, &tab.name, filter),
            &store::StoredGrid {
                limit: Some(*limit),
                filter: filter.clone(),
                showing_structure: *showing_structure,
                order_by: sort
                    .iter()
                    .map(|key| (key.expression.clone(), key.ascending))
                    .collect(),
                ..grid
            },
        );
    }
}

/// The id the next new buffer gets.
///
/// `stored` is what the profile last wrote, and the maximum over the open tabs
/// is the floor: a profile written before the field was kept has none, and one
/// written by a build that derived it could hand out an id a tab already holds.
/// Never the derived value alone -- that decreases when the highest tab closes,
/// and the reused id would hydrate the closed tab's snapshot.
fn next_query_id(stored: u64, tabs: &[store::StoredQueryTab]) -> u64 {
    stored.max(tabs.iter().map(|tab| tab.id + 1).max().unwrap_or(0))
}

/// Removing an entry below the active one shifts the vector under the index,
/// so clamping to the new length alone silently activates the wrong profile.
fn active_after_removal(active: usize, removed: usize, remaining: usize) -> usize {
    let shifted = if removed < active { active - 1 } else { active };
    shifted.min(remaining.saturating_sub(1))
}

/// What a removal took with it. The count is named because saved queries are
/// the one thing a person could still want back, and a directory that outlived
/// its profile is reported rather than passed over -- the id is derived from the
/// name, so whatever is left there attaches itself to the next profile called
/// the same thing.
fn removal_note(name: &str, queries: usize, problem: Option<String>) -> String {
    if let Some(problem) = problem {
        return format!("Removed {name}, but its saved queries are still on disk: {problem}");
    }

    match queries {
        0 => format!("Removed {name}."),
        1 => format!("Removed {name} and its saved query."),
        _ => format!("Removed {name} and its {queries} saved queries."),
    }
}

fn adjusted_editor_font_size(current: f32, delta: f32) -> f32 {
    (current + delta).clamp(EDITOR_FONT_SIZE_MIN, EDITOR_FONT_SIZE_MAX)
}

/// A zoom read back from disk. Clamped rather than trusted, because
/// `profiles.toml` is a text file: a size outside the range the controls offer
/// would otherwise be unreachable by the controls that set it. The finiteness
/// check is not decoration -- `clamp` on a NaN returns the NaN.
fn restored_editor_font_size(stored: Option<f32>) -> f32 {
    stored
        .filter(|size| size.is_finite())
        .map(|size| size.clamp(EDITOR_FONT_SIZE_MIN, EDITOR_FONT_SIZE_MAX))
        .unwrap_or(EDITOR_FONT_SIZE_DEFAULT)
}

/// A theme read back from disk, by name. An absent or unknown name is the
/// default: a palette dropped from `all` between launches must not strand the
/// app on a name nothing answers to.
fn restored_theme(name: Option<&str>) -> Theme {
    name.and_then(|name| Theme::all().into_iter().find(|theme| theme.name == name))
        .unwrap_or_default()
}

/// Push a theme everywhere it is read from. Only a glass theme wants the
/// desktop behind it, and the platform tears the vibrant view out of the window
/// the moment this says otherwise -- so it has to be said again on every
/// switch, not once at startup.
fn install_theme(theme: Theme, window: &mut Window, cx: &mut App) {
    theme.apply_to_components(cx);
    cx.set_global(theme);
    window.set_background_appearance(theme.window_background());
}

/// Push a font choice everywhere it is read from: the global the views render
/// against, and gpui-component's own theme, which carries the chrome family.
fn install_fonts(picked: Fonts, cx: &mut App) {
    cx.set_global(picked);
    let theme = *theme(cx);
    theme.apply_to_components(cx);
}

/// Fonts read back from disk. A family the text system cannot resolve falls
/// back to the default rather than being trusted: gpui matches a family it does
/// not know to nothing, so a font uninstalled between launches would otherwise
/// render the surface it was picked for blank.
fn restored_fonts(stored: Option<store::StoredFonts>, available: &[String]) -> Fonts {
    let stored = stored.unwrap_or_default();
    let pick = |family: Option<String>, default: &'static str| -> gpui::SharedString {
        family
            .filter(|family| available.iter().any(|name| name == family))
            .map_or_else(|| default.into(), gpui::SharedString::from)
    };
    Fonts {
        chrome: pick(stored.chrome, Fonts::DEFAULT_CHROME),
        editor: pick(stored.editor, Fonts::DEFAULT_EDITOR),
        grid: pick(stored.grid, Fonts::DEFAULT_GRID),
    }
}

fn editor_zoom_percent(font_size: f32) -> u32 {
    (font_size / EDITOR_FONT_SIZE_DEFAULT * 100.0).round() as u32
}

fn result_pane_is_expanded(query: &QueryState) -> bool {
    !matches!(query, QueryState::Idle)
}

/// Where a profile's connection details came from, which is what decides
/// whether its password is a saved credential.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Origin {
    Environment,
    Form,
}

/// The password that earns a Keychain entry, if any.
///
/// A file engine has none. A blank one is valid and never warned about, but an
/// empty Keychain item records nothing and is not written. And a password read
/// out of the environment is ephemeral by the convention that put it there --
/// copying it into the login Keychain would outlive the shell that set it, and
/// the session it belongs to already holds it in the config.
fn password_to_persist(config: &ConnectionConfig, origin: Origin) -> Option<&str> {
    if origin == Origin::Environment {
        return None;
    }
    config
        .server()
        .map(|server| server.password.as_str())
        .filter(|password| !password.is_empty())
}

fn connection_config_from_environment() -> Result<Option<ConnectionConfig>, String> {
    let host = std::env::var("PGHOST").ok();
    let port = std::env::var("PGPORT").ok();
    let database = std::env::var("PGDATABASE").ok();
    let user = std::env::var("PGUSER").ok();

    if [&host, &port, &database, &user]
        .iter()
        .all(|value| value.is_none())
    {
        return Ok(None);
    }

    let missing = [
        ("PGHOST", &host),
        ("PGDATABASE", &database),
        ("PGUSER", &user),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.is_none().then_some(name))
    .collect::<Vec<_>>();

    // Destructured rather than unwrapped, so the compiler -- not a list of
    // names twenty lines up -- is what guarantees these are present.
    let (Some(host), Some(database), Some(user)) = (host, database, user) else {
        return Err(format!(
            "Connection configuration is missing {}.",
            missing.join(", ")
        ));
    };

    let sslmode = match std::env::var("PGSSLMODE") {
        Ok(sslmode) => SslMode::parse(&sslmode)?,
        Err(_) => SslMode::default(),
    };
    let root_certificate = std::env::var("PGSSLROOTCERT")
        .ok()
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty());

    let port = port
        .map(|port| {
            port.parse()
                .map_err(|_| "PGPORT is not a valid port.".to_string())
        })
        .transpose()?;

    // The `PG*` variables configure a Postgres profile and are not generalised.
    // Slate is a generic client, not a generic environment reader, and there is
    // no convention for the other engines to read.
    Ok(Some(ConnectionConfig::Postgres(ServerConfig {
        host,
        port,
        database,
        user,
        password: std::env::var("PGPASSWORD").unwrap_or_default(),
        sslmode,
        root_certificate,
        // No `PG*` variable means it, and Slate is not inventing one.
        statement_timeout: 0,
    })))
}

/// Where a panic goes when nobody is watching stderr. A `.app` is spawned by
/// launchd, so a panic message lands in the unified log and the abort that
/// follows writes an `.ips` trace that does not carry it -- every stranger's
/// bug report would read "it closed" with nothing to read after it. Installed
/// first thing in `main`, because the deaths hardest to guess at from outside
/// are the ones before a window exists.
///
/// ponytail: one appended file, never rotated. A few kilobytes per crash; if
/// that ever becomes a real number, truncate on open past some size.
fn install_panic_log() {
    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        return;
    };
    let directory = PathBuf::from(home).join("Library/Logs/Slate");
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Nothing in here may panic: a panic inside the hook aborts with less
        // to read than the one it was called for. Hence every result dropped
        // rather than unwrapped.
        use std::io::Write as _;
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default();
        if std::fs::create_dir_all(&directory).is_ok()
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join("panic.log"))
        {
            let _ = writeln!(
                file,
                "--- {at} (unix seconds)\n{info}\n{}",
                std::backtrace::Backtrace::force_capture()
            );
        }
        previous(info);
    }));
}

fn main() {
    install_panic_log();
    Application::new().with_assets(Icons).run(|cx: &mut App| {
        cx.text_system()
            .add_fonts(
                guic_gpui_assets::BUNDLED_FONTS
                    .iter()
                    .map(|font| Cow::Borrowed(*font))
                    .collect(),
            )
            .expect("bundled fonts must be loadable");
        gpui_component::init(cx);
        // The defaults stand in until `Workspace::new` has read the file; the
        // theme is applied through them, and `apply_to_components` reads the
        // global rather than naming a family itself.
        cx.set_global(Fonts::default());
        let theme = Theme::default();
        theme.apply_to_components(cx);
        cx.set_global(theme);
        cx.bind_keys([
            KeyBinding::new("cmd-enter", RunQuery, None),
            KeyBinding::new("cmd-s", SaveQuery, None),
            KeyBinding::new("cmd-t", NewQuery, None),
            KeyBinding::new("cmd-shift-n", NewConnection, None),
            KeyBinding::new("cmd-w", CloseTab, None),
            KeyBinding::new("ctrl-tab", NextTab, None),
            KeyBinding::new("ctrl-shift-tab", PreviousTab, None),
            KeyBinding::new("ctrl-`", NextProfile, None),
            KeyBinding::new("ctrl-shift-`", PreviousProfile, None),
            KeyBinding::new("escape", ShowEditor, None),
            KeyBinding::new("cmd-shift-t", CycleTheme, None),
            KeyBinding::new("cmd-,", OpenSettings, None),
            KeyBinding::new("cmd-p", FuzzyOpen, None),
            KeyBinding::new("cmd-shift-p", CommandPalette, None),
            // A binding wins the keystroke at the deepest context it matches,
            // and the palette's search field is deeper than the list that binds
            // the arrows for itself -- a single-line input swallows them and
            // passes nothing on. `Palette > Input` matches at the field itself,
            // which is the only depth that takes them back, and it matches
            // nowhere else, so every other input keeps its arrows.
            KeyBinding::new("up", PalettePrevious, Some("Palette > Input")),
            KeyBinding::new("down", PaletteNext, Some("Palette > Input")),
            KeyBinding::new("cmd-+", ZoomEditorIn, None),
            KeyBinding::new("cmd-=", ZoomEditorIn, None),
            KeyBinding::new("cmd--", ZoomEditorOut, None),
            KeyBinding::new("cmd-0", ResetEditorZoom, None),
            // Scoped to the grid: `enter` everywhere else already belongs to
            // whatever is focused. Applying the edits has no binding at all --
            // `cmd+enter` runs the statement under the cursor and nothing else.
            KeyBinding::new("enter", EditCell, Some("Table")),
            // Scoped the same way, and for the same reason it has to be scoped
            // at all: an open input is deeper in the dispatch path, so its own
            // `cmd+c` wins there and the grid's copy never steals a text
            // selection.
            KeyBinding::new("cmd-c", CopyCell, Some("Table")),
            // Scoped to the grid like the two above, but unlike them it has to
            // keep working with a cell's input open, which is where the NULL
            // affordance beside it dispatches the same action from. A `ctrl`
            // stroke because a text field owns every `cmd` letter it is given.
            KeyBinding::new("ctrl-shift-n", SetNull, Some("Table")),
            // The input binds `tab` to indent and never asks its own
            // completion popup first, so the popup would never see the
            // keystroke. Scoped to the buffer, and registered after
            // `gpui_component::init`, which is what makes it win there and
            // nowhere else.
            KeyBinding::new("tab", AcceptCompletion, Some("Editor > Input")),
            KeyBinding::new("cmd-shift-s", ToggleSidebar, None),
            KeyBinding::new("cmd-q", Quit, None),
        ]);

        // An application menu is what actually makes `cmd+q` quit: the menu bar
        // owns the keystroke at the AppKit level, so it fires whatever has
        // focus, including a native text field that swallows the rest. Set
        // after the bindings, because the shortcut the item displays is read
        // back out of the keymap.
        //
        // The rest of the bar is there for the same reason in reverse: every
        // item names an action Slate already dispatches, so the menu is a way
        // to discover the keystroke rather than a second path to the work. No
        // Edit menu -- Slate does not own cut, copy and paste, the focused
        // field does, and a menu claiming them would take them from it.
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.set_menus(vec![
            Menu {
                name: "Slate".into(),
                items: vec![
                    MenuItem::action("Settings…", OpenSettings),
                    MenuItem::separator(),
                    MenuItem::action("Quit Slate", Quit),
                ],
            },
            Menu {
                name: "File".into(),
                items: vec![
                    MenuItem::action("New Query", NewQuery),
                    MenuItem::action("New Connection", NewConnection),
                    MenuItem::separator(),
                    MenuItem::action("Save Query", SaveQuery),
                    MenuItem::separator(),
                    MenuItem::action("Close Tab", CloseTab),
                ],
            },
            Menu {
                name: "Query".into(),
                items: vec![
                    MenuItem::action("Run", RunQuery),
                    MenuItem::action("Cancel", CancelQuery),
                ],
            },
            Menu {
                name: "View".into(),
                items: vec![
                    MenuItem::action("Toggle Sidebar", ToggleSidebar),
                    MenuItem::separator(),
                    MenuItem::action("Zoom In", ZoomEditorIn),
                    MenuItem::action("Zoom Out", ZoomEditorOut),
                    MenuItem::action("Reset Zoom", ResetEditorZoom),
                    MenuItem::separator(),
                    MenuItem::action("Cycle Theme", CycleTheme),
                ],
            },
        ]);

        // The platform titlebar is kept only for its window buttons: a system
        // bar in its own grey above Slate's chrome is the seam every native app
        // avoids. Slate paints that strip itself, and the buttons sit over it.
        let options = WindowOptions {
            window_background: theme.window_background(),
            titlebar: Some(TitlebarOptions {
                title: Some("Slate".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(
                    px(layout::SPACE_MD),
                    px((layout::TITLEBAR_HEIGHT - TRAFFIC_LIGHT_DIAMETER) / 2.),
                )),
            }),
            ..Default::default()
        };

        // Root must be the window's first layer or dialog and notification
        // layers panic when they look for it.
        cx.open_window(options, |window, cx| {
            let workspace = cx.new(|cx| Workspace::new(window, cx));
            cx.new(|cx| Root::new(workspace, window, cx))
        })
        .expect("failed to open window");

        // Slate has one window and no way to open a second: with it closed the
        // Dock icon is inert and the menu offers only Quit, which is an
        // application nobody can get back into. Quitting is the way back --
        // clicking the dead icon then launches Slate again, restoring the
        // profiles and the buffers `Workspace::on_release` has just written.
        // Reopening a window here would have to rebuild that same state anyway,
        // and would keep a process alive that is holding nothing.
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_password_is_never_copied_into_the_keychain() {
        let server = |password: &str| ServerConfig {
            host: "db.example".to_string(),
            port: None,
            database: "app".to_string(),
            user: "slate".to_string(),
            password: password.to_string(),
            sslmode: SslMode::default(),
            root_certificate: None,
            statement_timeout: 0,
        };
        let typed = ConnectionConfig::Postgres(server("hunter2"));
        assert_eq!(password_to_persist(&typed, Origin::Form), Some("hunter2"));
        // `PGPASSWORD` belongs to the shell that set it.
        assert_eq!(password_to_persist(&typed, Origin::Environment), None);
        // Blank is a valid password; an empty keychain item is not how one is
        // recorded.
        assert_eq!(
            password_to_persist(&ConnectionConfig::MySql(server("")), Origin::Form),
            None
        );
        assert_eq!(
            password_to_persist(
                &ConnectionConfig::Sqlite {
                    path: "/tmp/slate.db".to_string(),
                    statement_timeout: 0
                },
                Origin::Form
            ),
            None
        );
    }

    #[test]
    fn removing_a_profile_keeps_the_same_one_active() {
        assert_eq!(active_after_removal(2, 0, 3), 1);
        assert_eq!(active_after_removal(2, 2, 3), 2);
        assert_eq!(active_after_removal(2, 3, 3), 2);
        // The active profile was last, so there is nothing at its index now.
        assert_eq!(active_after_removal(2, 2, 2), 1);
        assert_eq!(active_after_removal(0, 0, 0), 0);
    }

    #[test]
    fn a_removal_says_what_went_with_the_profile() {
        assert_eq!(removal_note("Prod", 0, None), "Removed Prod.");
        assert_eq!(
            removal_note("Prod", 1, None),
            "Removed Prod and its saved query."
        );
        assert_eq!(
            removal_note("Prod", 7, None),
            "Removed Prod and its 7 saved queries."
        );
        // The count is not mentioned when the files are still there to count.
        assert_eq!(
            removal_note("Prod", 7, Some("permission denied".into())),
            "Removed Prod, but its saved queries are still on disk: permission denied"
        );
    }

    #[test]
    fn editor_zoom_stays_inside_its_readable_range() {
        assert_eq!(
            adjusted_editor_font_size(EDITOR_FONT_SIZE_MAX, EDITOR_FONT_SIZE_STEP),
            EDITOR_FONT_SIZE_MAX
        );
        assert_eq!(
            adjusted_editor_font_size(EDITOR_FONT_SIZE_MIN, -EDITOR_FONT_SIZE_STEP),
            EDITOR_FONT_SIZE_MIN
        );
        assert_eq!(
            adjusted_editor_font_size(EDITOR_FONT_SIZE_DEFAULT, EDITOR_FONT_SIZE_STEP),
            EDITOR_FONT_SIZE_DEFAULT + EDITOR_FONT_SIZE_STEP
        );
    }

    #[test]
    fn a_restored_zoom_is_clamped_rather_than_trusted() {
        // `profiles.toml` is a text file. A size outside the range the controls
        // offer would be a zoom the zoom controls cannot undo, and a NaN would
        // survive `clamp` and reach the text system.
        assert_eq!(restored_editor_font_size(None), EDITOR_FONT_SIZE_DEFAULT);
        assert_eq!(
            restored_editor_font_size(Some(f32::NAN)),
            EDITOR_FONT_SIZE_DEFAULT
        );
        assert_eq!(
            restored_editor_font_size(Some(f32::INFINITY)),
            EDITOR_FONT_SIZE_DEFAULT
        );
        assert_eq!(restored_editor_font_size(Some(900.0)), EDITOR_FONT_SIZE_MAX);
        assert_eq!(restored_editor_font_size(Some(0.0)), EDITOR_FONT_SIZE_MIN);
        assert_eq!(
            restored_editor_font_size(Some(EDITOR_FONT_SIZE_DEFAULT + EDITOR_FONT_SIZE_STEP)),
            EDITOR_FONT_SIZE_DEFAULT + EDITOR_FONT_SIZE_STEP
        );
    }

    #[test]
    fn default_editor_size_is_reported_as_one_hundred_percent() {
        assert_eq!(editor_zoom_percent(EDITOR_FONT_SIZE_DEFAULT), 100);
    }

    #[test]
    fn result_pane_expands_as_soon_as_a_query_starts() {
        assert!(!result_pane_is_expanded(&QueryState::Idle));
        assert!(result_pane_is_expanded(&QueryState::Running));
    }

    #[test]
    fn the_next_buffer_id_never_goes_backwards_over_a_closed_tab() {
        let tabs = |ids: &[u64]| {
            ids.iter()
                .map(|id| store::StoredQueryTab {
                    id: *id,
                    name: None,
                    active: false,
                })
                .collect::<Vec<_>>()
        };

        // Tab 1 closed, so the highest surviving id is 0 -- and deriving the
        // next id from it would hand out 1 again, over tab 1's snapshot.
        assert_eq!(next_query_id(2, &tabs(&[0])), 2);
        // A profile written before the id was persisted has no stored value,
        // and the derived one is all there is.
        assert_eq!(next_query_id(0, &tabs(&[0, 1])), 2);
        // A stored value behind the open tabs -- an older build's file beside
        // a newer build's tabs -- must not hand out a live id.
        assert_eq!(next_query_id(1, &tabs(&[0, 4])), 5);
        assert_eq!(next_query_id(0, &tabs(&[])), 0);
    }

    fn pending_row(sets: &[(&str, Option<&str>)], keys: &[(&str, &str)]) -> PendingRow {
        fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(column, value)| (column.to_string(), value.to_string()))
                .collect()
        }
        PendingRow {
            schema: "public".to_string(),
            table: "accounts".to_string(),
            sets: sets
                .iter()
                .map(|(column, value)| (column.to_string(), value.map(str::to_string)))
                .collect(),
            keys: owned(keys),
        }
    }

    #[test]
    fn a_nulled_cell_reaches_the_batch_as_the_keyword() {
        let rows = vec![pending_row(&[("name", None)], &[("id", "1")])];
        assert_eq!(
            update_batch(Engine::Postgres, &rows).unwrap(),
            "UPDATE \"public\".\"accounts\" SET \"name\" = NULL WHERE \"id\" = '1';"
        );
    }

    #[test]
    fn several_pending_rows_become_one_semicolon_joined_batch() {
        let rows = vec![
            pending_row(&[("name", Some("Ada"))], &[("id", "1")]),
            pending_row(&[("name", Some("Bo"))], &[("id", "2")]),
        ];

        let batch = update_batch(Engine::Postgres, &rows).unwrap();
        assert_eq!(
            batch,
            "UPDATE \"public\".\"accounts\" SET \"name\" = 'Ada' WHERE \"id\" = '1';\n\
             UPDATE \"public\".\"accounts\" SET \"name\" = 'Bo' WHERE \"id\" = '2';"
        );
        // The batch Slate builds has to pass the same gate Slate checks every
        // generated statement against, or the generator and the gate have
        // drifted apart.
        assert!(sql::is_generated_write(&batch));
    }

    #[test]
    fn an_engine_without_an_implicit_transaction_gets_explicit_brackets() {
        // MySQL and SQLite commit each statement on its own, so an unbracketed
        // batch could apply half the user's edits and report the failure of the
        // rest.
        let rows = vec![
            pending_row(&[("name", Some("Ada"))], &[("id", "1")]),
            pending_row(&[("name", Some("Bo"))], &[("id", "2")]),
        ];

        for engine in [Engine::MySql, Engine::Sqlite] {
            let batch = update_batch(engine, &rows).unwrap();
            assert!(batch.starts_with("BEGIN;\n"), "{engine:?} {batch}");
            assert!(batch.ends_with("\nCOMMIT;"), "{engine:?} {batch}");
            assert!(sql::is_generated_write(&batch), "{engine:?} {batch}");

            // One statement is already atomic, so brackets round it would be
            // ceremony the user has to read past.
            let single = update_batch(engine, &rows[..1]).unwrap();
            assert!(!single.contains("BEGIN"), "{engine:?} {single}");
            assert!(sql::is_generated_write(&single), "{engine:?} {single}");
        }

        let postgres = update_batch(Engine::Postgres, &rows).unwrap();
        assert!(!postgres.contains("BEGIN"), "{postgres}");
    }

    #[test]
    fn a_row_with_no_key_to_find_it_by_refuses_the_whole_batch() {
        let rows = vec![
            pending_row(&[("name", Some("Ada"))], &[("id", "1")]),
            // No keys at all: sql::update_row refuses this one, since there is
            // nothing to identify the row it would touch.
            pending_row(&[("name", Some("Bo"))], &[]),
        ];

        assert!(
            sql::update_row(
                Engine::Postgres,
                "public",
                "accounts",
                &[("name", Some("Bo"))],
                &[]
            )
            .is_none()
        );
        assert_eq!(update_batch(Engine::Postgres, &rows), None);
    }

    #[test]
    fn an_empty_batch_of_rows_has_nothing_to_send() {
        assert_eq!(update_batch(Engine::Postgres, &[]), None);
    }

    #[test]
    fn a_statement_run_again_moves_to_the_front_rather_than_doubling() {
        let mut history = vec!["SELECT 2".to_string(), "SELECT 1".to_string()];
        remember_statement(&mut history, "SELECT 1");

        assert_eq!(history, ["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn a_recalled_statement_starts_on_the_line_the_cursor_is_sent_to() {
        // The line `recall_statement` computes, against the text it computes it
        // from. A cursor on the wrong line runs the wrong statement.
        for (buffer, recalled) in [
            ("", "SELECT 1"),
            ("SELECT 2", "SELECT 1"),
            ("SELECT 2;\n", "SELECT\n  1"),
        ] {
            let appended = appended_statement(buffer, recalled);
            let line = appended.lines().count() - recalled.lines().count();

            assert_eq!(
                appended.lines().nth(line),
                recalled.lines().next(),
                "{appended:?}"
            );
        }
    }

    #[test]
    fn an_unterminated_buffer_is_terminated_before_the_appended_statement() {
        // Without the semicolon, "SELECT 1" and "UPDATE ..." would read back
        // as a single statement, and cmd+enter would send both at once.
        assert_eq!(
            appended_statement("SELECT 1", "UPDATE t SET a = 1"),
            "SELECT 1;\n\nUPDATE t SET a = 1"
        );
    }

    #[test]
    fn an_already_terminated_buffer_keeps_a_single_semicolon() {
        assert_eq!(
            appended_statement("SELECT 1;", "UPDATE t SET a = 1"),
            "SELECT 1;\n\nUPDATE t SET a = 1"
        );
    }

    #[test]
    fn an_empty_buffer_yields_just_the_statement() {
        assert_eq!(
            appended_statement("", "UPDATE t SET a = 1"),
            "UPDATE t SET a = 1"
        );
    }

    #[test]
    fn trailing_whitespace_in_the_buffer_does_not_ragged_the_join() {
        assert_eq!(
            appended_statement("SELECT 1\n\n  ", "UPDATE t SET a = 1"),
            "SELECT 1;\n\nUPDATE t SET a = 1"
        );
    }

    #[test]
    fn a_stored_font_family_survives_only_while_it_is_still_installed() {
        // The file is the user's to edit and the font is theirs to uninstall,
        // and gpui draws an unresolvable family as nothing at all -- so a name
        // that is gone has to read back as the default, not as blank text.
        let available = ["Lilex".to_string(), "SF Mono".to_string()];
        let restored = restored_fonts(
            Some(store::StoredFonts {
                chrome: Some("Uninstalled Sans".into()),
                editor: Some("SF Mono".into()),
                grid: None,
            }),
            &available,
        );

        assert_eq!(restored.chrome, Fonts::DEFAULT_CHROME);
        assert_eq!(restored.editor, "SF Mono");
        assert_eq!(restored.grid, Fonts::DEFAULT_GRID);
        assert_eq!(restored_fonts(None, &available), Fonts::default());
    }
}
