mod db;
mod explorer;
mod result_grid;
mod sql;

mod theme;

use std::{collections::HashMap, sync::Arc};

use gpui::{
    AnyElement, App, AppContext, Application, ClickEvent, Context, Entity, EntityInputHandler,
    Focusable,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, Render, StatefulInteractiveElement,
    Styled, Window, WindowOptions, actions, div, px,
};
use gpui_component::{
    Disableable, Root, Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    list::ListItem,
    table::{Table, TableState},
    tree::{TreeState, tree as render_tree},
};

use db::{
    Catalog, Connection, ConnectionConfig, DbError, Routine, RoutineKind, Structure,
};
use explorer::{ExplorerTarget, preview_sql, tree as build_explorer_tree};
use result_grid::ResultGrid;
use sql::Buffer;
use theme::{Appearance, Theme, layout, theme};

actions!(slate, [RunQuery, ShowEditor]);

const RETURN_HINT: &str = "esc returns to the editor";

enum ConnectionState {
    NotConfigured,
    Connecting { endpoint: String },
    Connected(Profile),
    Failed(String),
}

/// A connection and everything it owns.
///
/// The editor, results, explorer and query state live here rather than on
/// `Workspace` deliberately (spec §3.1). A profile is replaced wholesale when
/// the connection changes, so a buffer written against one database cannot be
/// retargeted at another — it does not exist outside its profile.
struct Profile {
    name: String,
    config: ConnectionConfig,
    connection: Connection,
    catalog: CatalogState,
    session: Session,
}

/// The per-profile view state.
///
/// Separate from `Profile` only because GPUI entities need a `&mut Window` to
/// create, and the connection resolves on a task that has none — so this is
/// built before the spawn and moved in once the connection opens.
struct Session {
    editor: Entity<InputState>,
    results: Entity<TableState<ResultGrid>>,
    query: QueryState,
    content: Content,
    explorer_filter: Entity<InputState>,
    explorer_tree: Entity<TreeState>,
    explorer_targets: Arc<HashMap<String, ExplorerTarget>>,
    /// `cmd+enter` reaches the workspace only through the focused element's
    /// dispatch path, so an unfocused editor makes the primary keystroke dead.
    editor_needs_focus: bool,
}

impl Session {
    fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let explorer_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter database objects…"));
        cx.subscribe(&explorer_filter, |workspace, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                workspace.refresh_explorer(cx);
            }
        })
        .detach();

        Self {
            editor: cx.new(|cx| {
                InputState::new(window, cx)
                    .code_editor("sql")
                    .placeholder("Write SQL…")
            }),
            results: cx.new(|cx| {
                TableState::new(ResultGrid::empty(), window, cx)
                    .sortable(false)
                    .col_movable(false)
                    .row_selectable(true)
                    .col_selectable(true)
            }),
            query: QueryState::Idle,
            content: Content::Query,
            explorer_filter,
            explorer_tree: cx.new(|cx| TreeState::new(cx)),
            explorer_targets: Arc::new(HashMap::new()),
            editor_needs_focus: true,
        }
    }
}

enum CatalogState {
    Loading,
    Loaded(Catalog),
    Failed(String),
}

struct RoutineDetails {
    schema: String,
    routine: Routine,
}

/// What the main pane is showing. One field rather than a set of mode flags:
/// the editor must never be hidden while `cmd+enter` still runs its contents,
/// and a generated preview must never be written over the user's buffer.
enum Content {
    Query,
    Preview(Preview),
    Routine(RoutineDetails),
}

/// An opened relation: the generated `SELECT` above the grid, plus the
/// relation's definition behind the Structure tab (spec §3.2).
struct Preview {
    schema: String,
    relation: String,
    sql: String,
    showing_structure: bool,
    structure: StructureState,
}

enum StructureState {
    Loading,
    Loaded(Structure),
    Failed(String),
}

struct ConnectionForm {
    url: Entity<InputState>,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    error: Option<String>,
}

impl ConnectionForm {
    fn new(
        config: Option<&ConnectionConfig>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let value = |value: Option<&str>| value.unwrap_or_default().to_string();
        let url = cx.new(|cx| InputState::new(window, cx).placeholder("postgresql://…"));
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Display name")
                .default_value(value(config.map(|config| config.database.as_str())))
        });
        let host = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Host")
                .default_value(value(config.map(|config| config.host.as_str())))
        });
        let port = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Port (optional)")
                .default_value(
                    config
                        .and_then(|config| config.port)
                        .map(|port| port.to_string())
                        .unwrap_or_default(),
                )
        });
        let database = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Database")
                .default_value(value(config.map(|config| config.database.as_str())))
        });
        let user = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Username")
                .default_value(value(config.map(|config| config.user.as_str())))
        });
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Password (optional)")
                .default_value(value(config.map(|config| config.password.as_str())))
                .masked(true)
        });

        Self {
            url,
            name,
            host,
            port,
            database,
            user,
            password,
            error: None,
        }
    }

    fn config(&self, cx: &App) -> Result<(String, ConnectionConfig), String> {
        let read = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let name = read(&self.name);
        let host = read(&self.host);
        let database = read(&self.database);
        let user = read(&self.user);
        let port = read(&self.port);

        for (label, value) in [
            ("Display name", &name),
            ("Host", &host),
            ("Database", &database),
            ("Username", &user),
        ] {
            if value.is_empty() {
                return Err(format!("{label} is required."));
            }
        }

        let port = if port.is_empty() {
            None
        } else {
            Some(
                port.parse()
                    .map_err(|_| "Port must be a number from 1 to 65535.".to_string())?,
            )
        };

        Ok((
            name,
            ConnectionConfig {
                host,
                port,
                database,
                user,
                password: self.password.read(cx).unmask_value().to_string(),
            },
        ))
    }
}

enum QueryState {
    Idle,
    Running,
    Complete {
        rows: usize,
        bytes: usize,
        elapsed: std::time::Duration,
        rows_affected: Option<u64>,
    },
    Failed(DbError),
}

struct Workspace {
    /// The only state outside a profile: the connection lifecycle, the form
    /// that starts one, and the generation counter that invalidates stale work.
    connection: ConnectionState,
    connection_form: ConnectionForm,
    /// Bumped on every connection attempt. A spawned task captures the value it
    /// was issued under and drops its result if the connection has moved on,
    /// so one profile's catalog or rows can never land on another's.
    connection_generation: u64,
}

impl Workspace {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let environment_config = connection_config_from_environment();
        let connection_form = ConnectionForm::new(
            environment_config.as_ref().ok().and_then(Option::as_ref),
            window,
            cx,
        );

        let mut workspace = Self {
            connection: ConnectionState::NotConfigured,
            connection_form,
            connection_generation: 0,
        };

        match environment_config {
            Ok(Some(config)) => {
                workspace.begin_connect(config.database.clone(), config, window, cx)
            }
            Ok(None) => {}
            Err(message) => workspace.connection = ConnectionState::Failed(message),
        }

        workspace
    }

    fn profile(&self) -> Option<&Profile> {
        match &self.connection {
            ConnectionState::Connected(profile) => Some(profile),
            _ => None,
        }
    }

    fn profile_mut(&mut self) -> Option<&mut Profile> {
        match &mut self.connection {
            ConnectionState::Connected(profile) => Some(profile),
            _ => None,
        }
    }

    fn apply_connection_url(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let url = self.connection_form.url.read(cx).value();
        let config = match ConnectionConfig::from_url(url.trim()) {
            Ok(config) => config,
            Err(error) => {
                self.connection_form.error = Some(error);
                cx.notify();
                return;
            }
        };

        for (input, value) in [
            (&self.connection_form.name, config.database.clone()),
            (&self.connection_form.host, config.host),
            (
                &self.connection_form.port,
                config.port.map(|port| port.to_string()).unwrap_or_default(),
            ),
            (&self.connection_form.database, config.database),
            (&self.connection_form.user, config.user),
            (&self.connection_form.password, config.password),
        ] {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        self.connection_form.error = None;
        cx.notify();
    }

    fn connect(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.connection, ConnectionState::Connecting { .. }) {
            return;
        }

        let (name, config) = match self.connection_form.config(cx) {
            Ok(profile) => profile,
            Err(error) => {
                self.connection_form.error = Some(error);
                cx.notify();
                return;
            }
        };

        self.connection_form.error = None;
        self.begin_connect(name, config, window, cx);
    }

    fn begin_connect(
        &mut self,
        name: String,
        config: ConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connection_generation += 1;
        let generation = self.connection_generation;
        self.connection = ConnectionState::Connecting {
            endpoint: config.endpoint(),
        };
        cx.notify();

        // Built here, not in the continuation: the profile owns its editor and
        // grid, and creating those needs a Window the spawned task will not
        // have. Dropped unused if the connection fails.
        let session = Session::new(window, cx);
        let task_config = config.clone();
        let connection_task = cx
            .background_executor()
            .spawn(async move { Connection::open(task_config) });

        cx.spawn(async move |workspace, cx| {
            let result = connection_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    if workspace.connection_generation != generation {
                        return;
                    }
                    workspace.connection = match result {
                        Ok(connection) => ConnectionState::Connected(Profile {
                            name,
                            config,
                            connection,
                            catalog: CatalogState::Loading,
                            session,
                        }),
                        Err(error) => ConnectionState::Failed(error.message),
                    };
                    workspace.load_catalog(cx);
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn load_catalog(&mut self, cx: &mut Context<Self>) {
        let Some(connection) = self.profile().map(|profile| profile.connection.clone()) else {
            return;
        };
        let generation = self.connection_generation;
        let catalog_task = cx
            .background_executor()
            .spawn(async move { connection.catalog() });

        cx.spawn(async move |workspace, cx| {
            let result = catalog_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    if workspace.connection_generation != generation {
                        return;
                    }
                    if let Some(profile) = workspace.profile_mut() {
                        profile.catalog = match result {
                            Ok(catalog) => CatalogState::Loaded(catalog),
                            Err(error) => CatalogState::Failed(error.message),
                        };
                    }
                    workspace.refresh_explorer(cx);
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn refresh_explorer(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let filter = profile.session.explorer_filter.read(cx).value();
        let explorer = match &profile.catalog {
            CatalogState::Loaded(catalog) => build_explorer_tree(catalog, &filter),
            _ => explorer::ExplorerTree {
                items: Vec::new(),
                targets: HashMap::new(),
            },
        };
        let tree = profile.session.explorer_tree.clone();

        if let Some(profile) = self.profile_mut() {
            profile.session.explorer_targets = Arc::new(explorer.targets);
        }
        tree.update(cx, |tree, cx| tree.set_items(explorer.items, cx));
    }

    fn open_explorer_target(&mut self, target: ExplorerTarget, cx: &mut Context<Self>) {
        let Some(catalog) = self.catalog() else {
            return;
        };

        match target {
            ExplorerTarget::Relation {
                schema_index,
                relation_index,
            } => {
                let Some((schema, relation)) =
                    catalog.schemas.get(schema_index).and_then(|schema| {
                        let relation = schema.relations.get(relation_index)?;
                        Some((schema.name.clone(), relation.name.clone()))
                    })
                else {
                    return;
                };

                // The preview is shown in its own read-only surface rather than
                // written into the editor: `set_value` is not undoable, so that
                // would destroy an unsaved buffer on a misclick.
                let sql = preview_sql(&schema, &relation);
                if let Some(profile) = self.profile_mut() {
                    profile.session.content = Content::Preview(Preview {
                        schema: schema.clone(),
                        relation: relation.clone(),
                        sql: sql.clone(),
                        showing_structure: false,
                        structure: StructureState::Loading,
                    });
                }
                self.load_structure(schema, relation, cx);
                self.execute_sql(sql, cx);
            }
            ExplorerTarget::Routine {
                schema_index,
                routine_index,
            } => {
                let Some(details) = catalog.schemas.get(schema_index).and_then(|schema| {
                    let routine = schema.routines.get(routine_index)?.clone();
                    Some(RoutineDetails {
                        schema: schema.name.clone(),
                        routine,
                    })
                }) else {
                    return;
                };

                if let Some(profile) = self.profile_mut() {
                    profile.session.content = Content::Routine(details);
                }
                cx.notify();
            }
        }
    }

    fn load_structure(&mut self, schema: String, relation: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let connection = profile.connection.clone();
        let generation = self.connection_generation;
        let structure_task = cx.background_executor().spawn({
            let (schema, relation) = (schema.clone(), relation.clone());
            async move { connection.structure(&schema, &relation) }
        });

        cx.spawn(async move |workspace, cx| {
            let result = structure_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    if workspace.connection_generation != generation {
                        return;
                    }
                    // A second click while this was in flight has already
                    // replaced the surface, and one relation's columns under
                    // another's name is worse than no columns at all.
                    if let Some(profile) = workspace.profile_mut()
                        && let Content::Preview(preview) = &mut profile.session.content
                        && preview.schema == schema
                        && preview.relation == relation
                    {
                        preview.structure = match result {
                            Ok(structure) => StructureState::Loaded(structure),
                            Err(error) => StructureState::Failed(error.message),
                        };
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }

    fn show_structure(&mut self, showing_structure: bool, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut()
            && let Content::Preview(preview) = &mut profile.session.content
        {
            preview.showing_structure = showing_structure;
            cx.notify();
        }
    }

    fn catalog(&self) -> Option<&Catalog> {
        match self.profile().map(|profile| &profile.catalog) {
            Some(CatalogState::Loaded(catalog)) => Some(catalog),
            _ => None,
        }
    }

    /// Return to the editor. Without this the routine and preview surfaces are
    /// one-way doors, since they replace the editor entirely.
    fn show_editor(&mut self, _: &ShowEditor, _: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        if matches!(profile.session.content, Content::Query) {
            return;
        }
        profile.session.content = Content::Query;
        profile.session.editor_needs_focus = true;
        cx.notify();
    }

    fn run_query(&mut self, _: &RunQuery, window: &mut Window, cx: &mut Context<Self>) {
        // The editor is hidden behind the routine and preview surfaces, and
        // running SQL the user cannot see is how an unrelated statement left in
        // the buffer gets executed by muscle memory.
        if !matches!(
            self.profile().map(|profile| &profile.session.content),
            Some(Content::Query)
        ) {
            return;
        }

        let Some(sql) = self.sql_to_run(window, cx) else {
            if let Some(profile) = self.profile_mut() {
                profile.session.query = QueryState::Failed(DbError {
                    message: "There is no statement to run.".into(),
                    position: None,
                });
            }
            cx.notify();
            return;
        };

        self.execute_sql(sql, cx);
    }

    /// Run `sql` against the active profile.
    ///
    /// There is deliberately no "not connected" branch: SQL is only reachable
    /// through a profile's own editor or explorer, so the absence of one is not
    /// a state the user can be shown an error about.
    fn execute_sql(&mut self, sql: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        // Guarded here rather than in each caller: every path that runs SQL
        // routes through this one, and a caller that forgets would let two
        // results race into the grid with the older one landing last.
        if matches!(profile.session.query, QueryState::Running) {
            return;
        }

        let connection = profile.connection.clone();
        let results = profile.session.results.clone();
        profile.session.query = QueryState::Running;

        // Rows from the previous statement must not sit under the one now on
        // screen -- a reader cannot tell stale rows from fresh ones.
        results.update(cx, |table, cx| {
            *table.delegate_mut() = ResultGrid::empty();
            table.refresh(cx);
        });
        cx.notify();

        let generation = self.connection_generation;
        let query_task = cx
            .background_executor()
            .spawn(async move { connection.query(&sql) });

        cx.spawn(async move |workspace, cx| {
            let result = query_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    if workspace.connection_generation != generation {
                        return;
                    }
                    let Some(profile) = workspace.profile_mut() else {
                        return;
                    };

                    match result {
                        Ok(result) => {
                            profile.session.query = QueryState::Complete {
                                rows: result.rows.len(),
                                bytes: result.bytes,
                                elapsed: result.elapsed,
                                rows_affected: result.rows_affected,
                            };
                            let results = profile.session.results.clone();
                            results.update(cx, |table, cx| {
                                *table.delegate_mut() = ResultGrid::new(result);
                                table.refresh(cx);
                            });
                        }
                        Err(error) => profile.session.query = QueryState::Failed(error),
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn sql_to_run(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<String> {
        let editor = self.profile()?.session.editor.clone();
        let selection = editor.update(cx, |editor, cx| {
            let selection = editor.selected_text_range(false, window, cx)?;
            if selection.range.is_empty() {
                return None;
            }

            let mut adjusted_range = None;
            editor.text_for_range(selection.range, &mut adjusted_range, window, cx)
        });

        if selection.is_some() {
            return selection;
        }

        let editor = editor.read(cx);
        let sql = editor.value();
        let range = Buffer::parse(&sql).statement_at(editor.cursor())?;
        Some(sql[range].to_string())
    }

    fn render_connection_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *theme(cx);
        let connecting = matches!(self.connection, ConnectionState::Connecting { .. });
        let message = self.connection_form.error.as_ref().cloned().or_else(|| {
            if let ConnectionState::Failed(message) = &self.connection {
                Some(message.clone())
            } else {
                None
            }
        });

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(layout::DIALOG_WIDTH))
                    .p(px(layout::SPACE_LG))
                    .bg(t.surface)
                    .border_1()
                    .border_color(t.border)
                    .rounded(px(layout::RADIUS_PANEL))
                    .flex()
                    .flex_col()
                    .gap(px(layout::SPACE_MD))
                    .child(div().text_size(px(20.)).child("Connect to Postgres"))
                    .child(
                        div()
                            .text_color(t.text_muted)
                            .child("Paste a connection URL or enter the profile fields."),
                    )
                    .child(self.form_field("Connection URL", &self.connection_form.url))
                    .child(
                        div().flex().justify_end().child(
                            Button::new("apply-connection-url")
                                .label("Use URL")
                                .disabled(connecting)
                                .on_click(cx.listener(Self::apply_connection_url)),
                        ),
                    )
                    .child(self.form_field("Display name", &self.connection_form.name))
                    .child(self.form_field("Host", &self.connection_form.host))
                    .child(self.form_field("Port", &self.connection_form.port))
                    .child(self.form_field("Database", &self.connection_form.database))
                    .child(self.form_field("Username", &self.connection_form.user))
                    .child(self.form_field("Password", &self.connection_form.password))
                    .children(message.map(|message| div().text_color(t.danger).child(message)))
                    .child(
                        Button::new("connect")
                            .label(if connecting {
                                "Connecting…"
                            } else {
                                "Connect"
                            })
                            .primary()
                            .disabled(connecting)
                            .on_click(cx.listener(Self::connect)),
                    ),
            )
    }

    fn form_field(&self, label: &'static str, input: &Entity<InputState>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(layout::SPACE_XS))
            .child(label)
            .child(Input::new(input).w_full())
    }

    fn render_main_content(
        profile: &Profile,
        result_lines: Vec<String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = *theme(cx);
        let mono = gpui_component::Theme::global(cx).mono_font_family.clone();

        if let Content::Routine(details) = &profile.session.content {
            let kind = match details.routine.kind {
                RoutineKind::Function => "Function",
                RoutineKind::Procedure => "Procedure",
            };
            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .p(px(layout::SPACE_LG))
                        .border_b_1()
                        .border_color(t.border)
                        .flex()
                        .flex_col()
                        .gap(px(layout::SPACE_SM))
                        .child(div().text_size(px(18.)).child(format!(
                            "{}.{}({})",
                            details.schema,
                            details.routine.name,
                            details.routine.identity_arguments
                        )))
                        .child(
                            div()
                                .flex()
                                .gap(px(layout::SPACE_LG))
                                .text_color(t.text_muted)
                                .child(kind)
                                .child(format!("Language: {}", details.routine.language))
                                .children((!details.routine.result_type.is_empty()).then(|| {
                                    div().child(format!("Returns: {}", details.routine.result_type))
                                }))
                                .child(RETURN_HINT),
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
                        .child(details.routine.definition.clone()),
                );
        }

        // The generated preview is shown as its own read-only surface, so it is
        // always distinguishable from SQL the user wrote.
        let top = match &profile.session.content {
            Content::Preview(preview) => div()
                .flex_1()
                .min_h_0()
                .p(px(layout::SPACE_LG))
                .font_family(mono)
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_SM))
                .child(
                    div()
                        .text_color(t.text_muted)
                        .child(format!("Generated preview · {RETURN_HINT}")),
                )
                .child(preview.sql.clone())
                .child(
                    div()
                        .flex()
                        .gap(px(layout::SPACE_XS))
                        .child(Self::preview_tab("Data", !preview.showing_structure, cx))
                        .child(Self::preview_tab(
                            "Structure",
                            preview.showing_structure,
                            cx,
                        )),
                )
                .into_any_element(),
            _ => div()
                .flex_1()
                .min_h_0()
                .p(px(layout::SPACE_LG))
                .font_family(mono)
                .child(
                    Input::new(&profile.session.editor)
                        .h_full()
                        .appearance(false)
                        .bordered(false)
                        .focus_bordered(false),
                )
                .into_any_element(),
        };

        let bottom = match &profile.session.content {
            Content::Preview(preview) if preview.showing_structure => {
                Self::render_structure(&preview.structure, cx)
            }
            _ => div()
                .size_full()
                .child(Table::new(&profile.session.results).bordered(false))
                .children((!result_lines.is_empty()).then(|| {
                    div()
                        .size_full()
                        .p(px(layout::SPACE_LG))
                        .font_family(gpui_component::Theme::global(cx).mono_font_family.clone())
                        .text_color(if matches!(profile.session.query, QueryState::Failed(_)) {
                            t.danger
                        } else {
                            t.text
                        })
                        .children(
                            result_lines
                                .into_iter()
                                .map(|line| div().w_full().py(px(layout::SPACE_XS)).child(line)),
                        )
                }))
                .into_any_element(),
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(top)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_t_1()
                    .border_color(t.border)
                    .child(bottom),
            )
    }

    fn preview_tab(label: &'static str, selected: bool, cx: &mut Context<Self>) -> Button {
        let button = Button::new(label).label(label).small();
        let button = if selected {
            button.primary()
        } else {
            button.ghost()
        };
        button.on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
            workspace.show_structure(label == "Structure", cx);
        }))
    }

    fn render_structure(state: &StructureState, cx: &mut Context<Self>) -> AnyElement {
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
                .text_color(t.text_muted)
                .child(label)
        };
        let name_column = |name: String| div().w(px(220.)).min_w(px(220.)).child(name);
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
                            .child(column.data_type.clone()),
                    )
                    .child(
                        div()
                            .w(px(80.))
                            .min_w(px(80.))
                            .text_color(t.text_muted)
                            .child(if column.nullable { "nullable" } else { "not null" }),
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

    fn render_explorer(profile: &Profile, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *theme(cx);
        let workspace = cx.entity().downgrade();
        let targets = profile.session.explorer_targets.clone();
        let content = match &profile.catalog {
            CatalogState::Loading => div()
                .p(px(layout::SPACE_MD))
                .text_color(t.text_muted)
                .child("Loading database objects…")
                .into_any_element(),
            CatalogState::Failed(message) => div()
                .p(px(layout::SPACE_MD))
                .text_color(t.danger)
                .child(message.clone())
                .into_any_element(),
            CatalogState::Loaded(catalog) if catalog.schemas.is_empty() => div()
                .p(px(layout::SPACE_MD))
                .text_color(t.text_muted)
                .child("No database objects found.")
                .into_any_element(),
            CatalogState::Loaded(_) => {
                render_tree(&profile.session.explorer_tree, move |index, entry, _, _, cx| {
                let t = *theme(cx);
                let row = ListItem::new(index)
                    .pl(px(
                        layout::SPACE_SM + entry.depth() as f32 * layout::SPACE_MD
                    ))
                    .text_color(t.text)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(entry.item().label.clone()),
                    );
                let Some(target) = targets.get(entry.item().id.as_str()).copied() else {
                    return row;
                };
                let workspace = workspace.clone();
                row.on_click(move |_, _, cx| {
                    _ = workspace.update(cx, |workspace, cx| {
                        workspace.open_explorer_target(target, cx);
                    });
                    })
                })
                .into_any_element()
            }
        };

        div()
            .w(px(layout::SIDEBAR_DEFAULT_WIDTH))
            .min_w(px(layout::SIDEBAR_MIN_WIDTH))
            .h_full()
            .bg(t.surface)
            .border_r_1()
            .border_color(t.border)
            .flex()
            .flex_col()
            .child(
                div()
                    .p(px(layout::SPACE_SM))
                    .border_b_1()
                    .border_color(t.border)
                    .child(Input::new(&profile.session.explorer_filter).w_full()),
            )
            .child(div().flex_1().min_h_0().child(content))
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *theme(cx);
        // Deferred to render because the editor has to be mounted before it can
        // take focus, and the connection that reveals it resolves off-thread.
        let take_focus = self.profile_mut().and_then(|profile| {
            let wanted = profile.session.editor_needs_focus
                && matches!(profile.session.content, Content::Query);
            wanted.then(|| {
                profile.session.editor_needs_focus = false;
                profile.session.editor.clone()
            })
        });
        if let Some(editor) = take_focus {
            editor.focus_handle(cx).focus(window);
        }

        let (status, status_color) = match &self.connection {
            ConnectionState::NotConfigured => {
                ("No connection configured.".to_string(), t.text_muted)
            }
            ConnectionState::Connecting { endpoint } => {
                (format!("Connecting to {endpoint}…"), t.text_muted)
            }
            ConnectionState::Connected(profile) => (
                format!("{} · {}", profile.name, profile.config.endpoint()),
                t.success,
            ),
            ConnectionState::Failed(message) => (message.clone(), t.danger),
        };
        let Some(profile) = self.profile() else {
            return div()
                .id("connection-form")
                .size_full()
                .bg(t.bg)
                .text_color(t.text)
                .child(self.render_connection_form(cx));
        };

        let result_lines = match &profile.session.query {
            QueryState::Idle => vec!["⌘↵ runs the selection or statement under the cursor.".into()],
            QueryState::Running => vec!["Running query…".into()],
            QueryState::Failed(error) => {
                let position = error
                    .position
                    .map(|position| format!(" (at byte {position})"))
                    .unwrap_or_default();
                vec![format!("{}{position}", error.message)]
            }
            QueryState::Complete {
                rows,
                bytes,
                elapsed,
                rows_affected,
            } if *rows == 0 => vec![match rows_affected {
                Some(rows) => format!("Query completed. Server row count: {rows}."),
                None => "Query completed.".into(),
            }],
            QueryState::Complete { .. } => Vec::new(),
        };
        let query_status = match &profile.session.query {
            QueryState::Complete {
                rows,
                bytes,
                elapsed,
                ..
            } => Some(format!("{rows} row(s) · {bytes} bytes · {elapsed:.1?}")),
            _ => None,
        };

        div()
            .id("workspace")
            .on_action(cx.listener(Self::run_query))
            .on_action(cx.listener(Self::show_editor))
            .size_full()
            .bg(t.bg)
            .text_color(t.text)
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(layout::TITLEBAR_HEIGHT))
                    .w_full()
                    .bg(t.surface)
                    .border_b_1()
                    .border_color(t.border)
                    .flex()
                    .items_center()
                    .px(px(layout::SPACE_MD))
                    .child("Slate"),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(Self::render_explorer(profile, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(Self::render_main_content(profile, result_lines, cx)),
                    ),
            )
            .child(
                div()
                    .h(px(layout::STATUS_HEIGHT))
                    .w_full()
                    .bg(t.surface)
                    .border_t_1()
                    .border_color(t.border)
                    .flex()
                    .items_center()
                    .px(px(layout::SPACE_MD))
                    .text_color(status_color)
                    .child(status)
                    .children(query_status.map(|query_status| {
                        div().ml_auto().text_color(t.text_muted).child(query_status)
                    })),
            )
    }
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

    if let Ok(sslmode) = std::env::var("PGSSLMODE") {
        db::reject_unsupported_sslmode(&sslmode)?;
    }

    let port = port
        .map(|port| {
            port.parse()
                .map_err(|_| "PGPORT is not a valid port.".to_string())
        })
        .transpose()?;

    Ok(Some(ConnectionConfig {
        host,
        port,
        database,
        user,
        password: std::env::var("PGPASSWORD").unwrap_or_default(),
    }))
}

fn main() {
    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);
        let theme = Theme::new(Appearance::Dark);
        theme.apply_to_components(cx);
        cx.set_global(theme);
        cx.bind_keys([
            KeyBinding::new("cmd-enter", RunQuery, None),
            KeyBinding::new("escape", ShowEditor, None),
        ]);

        // Root must be the window's first layer or dialog and notification
        // layers panic when they look for it.
        cx.open_window(WindowOptions::default(), |window, cx| {
            let workspace = cx.new(|cx| Workspace::new(window, cx));
            cx.new(|cx| Root::new(workspace, window, cx))
        })
        .expect("failed to open window");

        cx.activate(true);
    });
}
