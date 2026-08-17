mod db;
mod explorer;
mod result_grid;
mod sql;

mod theme;

use gpui::{
    App, AppContext, Application, ClickEvent, Context, Entity, EntityInputHandler,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, Render, Styled, Window,
    WindowOptions, actions, div, px,
};
use gpui_component::{
    Disableable, Root,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    list::ListItem,
    table::{Table, TableState},
    tree::{TreeState, tree},
};

use db::{Catalog, Connection, ConnectionConfig, DbError};
use explorer::tree_items;
use result_grid::ResultGrid;
use sql::Buffer;
use theme::{Appearance, Theme, layout, theme};

actions!(slate, [RunQuery]);

enum ConnectionState {
    NotConfigured,
    Connecting { endpoint: String },
    Connected(Profile),
    Failed(String),
}

struct Profile {
    name: String,
    config: ConnectionConfig,
    connection: Connection,
    catalog: CatalogState,
}

enum CatalogState {
    Loading,
    Loaded(Catalog),
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
    connection: ConnectionState,
    connection_form: ConnectionForm,
    explorer_filter: Entity<InputState>,
    explorer_tree: Entity<TreeState>,
    editor: Entity<InputState>,
    results: Entity<TableState<ResultGrid>>,
    query: QueryState,
}

impl Workspace {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let environment_config = connection_config_from_environment();
        let connection_form = ConnectionForm::new(
            environment_config.as_ref().ok().and_then(Option::as_ref),
            window,
            cx,
        );
        let explorer_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter database objects…"));
        let explorer_tree = cx.new(|cx| TreeState::new(cx));
        cx.subscribe(&explorer_filter, |workspace, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                workspace.refresh_explorer(cx);
            }
        })
        .detach();
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("sql")
                .placeholder("Write SQL…")
        });
        let results = cx.new(|cx| {
            TableState::new(ResultGrid::empty(), window, cx)
                .sortable(false)
                .col_movable(false)
                .row_selectable(true)
                .col_selectable(true)
        });

        let config = match environment_config {
            Ok(Some(config)) => config,
            Ok(None) => {
                return Self {
                    connection: ConnectionState::NotConfigured,
                    connection_form,
                    explorer_filter,
                    explorer_tree,
                    editor,
                    results,
                    query: QueryState::Idle,
                };
            }
            Err(message) => {
                return Self {
                    connection: ConnectionState::Failed(message),
                    connection_form,
                    explorer_filter,
                    explorer_tree,
                    editor,
                    results,
                    query: QueryState::Idle,
                };
            }
        };

        let endpoint = config.endpoint();
        let task_config = config.clone();
        let connection_task = cx
            .background_executor()
            .spawn(async move { Connection::open(task_config) });

        cx.spawn(async move |workspace, cx| {
            let result = connection_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    workspace.connection = match result {
                        Ok(connection) => ConnectionState::Connected(Profile {
                            name: config.database.clone(),
                            config,
                            connection,
                            catalog: CatalogState::Loading,
                        }),
                        Err(error) => ConnectionState::Failed(error.message),
                    };
                    workspace.load_catalog(cx);
                    cx.notify();
                })
                .ok();
        })
        .detach();

        Self {
            connection: ConnectionState::Connecting { endpoint },
            connection_form,
            explorer_filter,
            explorer_tree,
            editor,
            results,
            query: QueryState::Idle,
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

    fn connect(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
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
        let endpoint = config.endpoint();
        let task_config = config.clone();
        let connection_task = cx
            .background_executor()
            .spawn(async move { Connection::open(task_config) });

        self.connection_form.error = None;
        self.connection = ConnectionState::Connecting {
            endpoint: endpoint.clone(),
        };
        cx.notify();

        cx.spawn(async move |workspace, cx| {
            let result = connection_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    workspace.connection = match result {
                        Ok(connection) => ConnectionState::Connected(Profile {
                            name,
                            config,
                            connection,
                            catalog: CatalogState::Loading,
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
        let connection = match &self.connection {
            ConnectionState::Connected(profile) => profile.connection.clone(),
            _ => return,
        };
        let catalog_task = cx
            .background_executor()
            .spawn(async move { connection.catalog() });

        cx.spawn(async move |workspace, cx| {
            let result = catalog_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    if let ConnectionState::Connected(profile) = &mut workspace.connection {
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
        let filter = self.explorer_filter.read(cx).value();
        let items = match &self.connection {
            ConnectionState::Connected(Profile {
                catalog: CatalogState::Loaded(catalog),
                ..
            }) => tree_items(catalog, &filter),
            _ => Vec::new(),
        };

        self.explorer_tree
            .update(cx, |tree, cx| tree.set_items(items, cx));
    }

    fn run_query(&mut self, _: &RunQuery, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.query, QueryState::Running) {
            return;
        }

        let connection = match &self.connection {
            ConnectionState::Connected(profile) => profile.connection.clone(),
            ConnectionState::NotConfigured => {
                self.query = QueryState::Failed(DbError {
                    message: "No connection is configured.".into(),
                    position: None,
                });
                cx.notify();
                return;
            }
            ConnectionState::Connecting { .. } => {
                self.query = QueryState::Failed(DbError {
                    message: "The connection is still opening.".into(),
                    position: None,
                });
                cx.notify();
                return;
            }
            ConnectionState::Failed(message) => {
                self.query = QueryState::Failed(DbError {
                    message: message.clone(),
                    position: None,
                });
                cx.notify();
                return;
            }
        };

        let Some(sql) = self.sql_to_run(window, cx) else {
            self.query = QueryState::Failed(DbError {
                message: "There is no statement to run.".into(),
                position: None,
            });
            cx.notify();
            return;
        };

        self.query = QueryState::Running;
        cx.notify();

        let query_task = cx
            .background_executor()
            .spawn(async move { connection.query(&sql) });

        cx.spawn(async move |workspace, cx| {
            let result = query_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    workspace.query = match result {
                        Ok(result) => {
                            let summary = QueryState::Complete {
                                rows: result.rows.len(),
                                bytes: result.bytes,
                                elapsed: result.elapsed,
                                rows_affected: result.rows_affected,
                            };
                            workspace.results.update(cx, |table, cx| {
                                *table.delegate_mut() = ResultGrid::new(result);
                                table.refresh(cx);
                            });
                            summary
                        }
                        Err(error) => QueryState::Failed(error),
                    };
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn sql_to_run(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<String> {
        let selection = self.editor.update(cx, |editor, cx| {
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

        let editor = self.editor.read(cx);
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
                    .w(px(layout::SIDEBAR_MAX_WIDTH))
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

    fn render_explorer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *theme(cx);
        let content = match &self.connection {
            ConnectionState::Connected(Profile {
                catalog: CatalogState::Loading,
                ..
            }) => div()
                .p(px(layout::SPACE_MD))
                .text_color(t.text_muted)
                .child("Loading database objects…")
                .into_any_element(),
            ConnectionState::Connected(Profile {
                catalog: CatalogState::Failed(message),
                ..
            }) => div()
                .p(px(layout::SPACE_MD))
                .text_color(t.danger)
                .child(message.clone())
                .into_any_element(),
            ConnectionState::Connected(Profile {
                catalog: CatalogState::Loaded(catalog),
                ..
            }) if catalog.schemas.is_empty() => div()
                .p(px(layout::SPACE_MD))
                .text_color(t.text_muted)
                .child("No database objects found.")
                .into_any_element(),
            ConnectionState::Connected(Profile {
                catalog: CatalogState::Loaded(_),
                ..
            }) => tree(&self.explorer_tree, |index, entry, _, _, cx| {
                let t = *theme(cx);
                ListItem::new(index)
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
                    )
            })
            .into_any_element(),
            _ => div().into_any_element(),
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
                    .child(Input::new(&self.explorer_filter).w_full()),
            )
            .child(div().flex_1().min_h_0().child(content))
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *theme(cx);
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
        let result_lines = match &self.query {
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
        let query_status = match &self.query {
            QueryState::Complete {
                rows,
                bytes,
                elapsed,
                ..
            } => Some(format!("{rows} row(s) · {bytes} bytes · {elapsed:.1?}")),
            _ => None,
        };

        if !matches!(self.connection, ConnectionState::Connected(_)) {
            return div()
                .id("connection-form")
                .size_full()
                .bg(t.bg)
                .text_color(t.text)
                .child(self.render_connection_form(cx));
        }

        div()
            .id("workspace")
            .on_action(cx.listener(Self::run_query))
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
                    .child(self.render_explorer(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .p(px(layout::SPACE_LG))
                                    .font_family(
                                        gpui_component::Theme::global(cx).mono_font_family.clone(),
                                    )
                                    .child(
                                        Input::new(&self.editor)
                                            .h_full()
                                            .appearance(false)
                                            .bordered(false)
                                            .focus_bordered(false),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .border_t_1()
                                    .border_color(t.border)
                                    .child(Table::new(&self.results).bordered(false))
                                    .children((!result_lines.is_empty()).then(|| {
                                        div()
                                            .size_full()
                                            .p(px(layout::SPACE_LG))
                                            .font_family(
                                                gpui_component::Theme::global(cx)
                                                    .mono_font_family
                                                    .clone(),
                                            )
                                            .text_color(
                                                if matches!(self.query, QueryState::Failed(_)) {
                                                    t.danger
                                                } else {
                                                    t.text
                                                },
                                            )
                                            .children(result_lines.into_iter().map(|line| {
                                                div().w_full().py(px(layout::SPACE_XS)).child(line)
                                            }))
                                    })),
                            ),
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

    if !missing.is_empty() {
        return Err(format!(
            "Connection configuration is missing {}.",
            missing.join(", ")
        ));
    }

    let port = port
        .map(|port| {
            port.parse()
                .map_err(|_| "PGPORT is not a valid port.".to_string())
        })
        .transpose()?;

    Ok(Some(ConnectionConfig {
        host: host.expect("checked above"),
        port,
        database: database.expect("checked above"),
        user: user.expect("checked above"),
        password: std::env::var("PGPASSWORD").unwrap_or_default(),
    }))
}

fn main() {
    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);
        let theme = Theme::new(Appearance::Dark);
        theme.apply_to_components(cx);
        cx.set_global(theme);
        cx.bind_keys([KeyBinding::new("cmd-enter", RunQuery, None)]);

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
