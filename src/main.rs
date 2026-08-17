mod db;
mod sql;

mod theme;

use gpui::{
    App, AppContext, Application, Context, Entity, EntityInputHandler, InteractiveElement,
    IntoElement, KeyBinding, ParentElement, Render, Styled, Window, WindowOptions, actions, div,
    px,
};
use gpui_component::{
    Root,
    input::{Input, InputState},
    scroll::ScrollableElement,
};

use db::{Connection, ConnectionConfig, DbError, QueryResult};
use sql::Buffer;
use theme::{Appearance, Theme, layout, theme};

actions!(slate, [RunQuery]);

const RESULT_PREVIEW_ROWS: usize = 100;

enum ConnectionState {
    NotConfigured,
    Connecting {
        endpoint: String,
    },
    Connected {
        connection: Connection,
        endpoint: String,
    },
    Failed(String),
}

enum QueryState {
    Idle,
    Running,
    Complete(QueryResult),
    Failed(DbError),
}

struct Workspace {
    connection: ConnectionState,
    editor: Entity<InputState>,
    query: QueryState,
}

impl Workspace {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("sql")
                .placeholder("Write SQL…")
        });

        let config = match connection_config_from_environment() {
            Ok(Some(config)) => config,
            Ok(None) => {
                return Self {
                    connection: ConnectionState::NotConfigured,
                    editor,
                    query: QueryState::Idle,
                };
            }
            Err(message) => {
                return Self {
                    connection: ConnectionState::Failed(message),
                    editor,
                    query: QueryState::Idle,
                };
            }
        };

        let endpoint = config.endpoint();
        let connection_task = cx
            .background_executor()
            .spawn(async move { Connection::open(config) });
        let task_endpoint = endpoint.clone();

        cx.spawn(async move |workspace, cx| {
            let result = connection_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    workspace.connection = match result {
                        Ok(connection) => ConnectionState::Connected {
                            connection,
                            endpoint: task_endpoint,
                        },
                        Err(error) => ConnectionState::Failed(error.message),
                    };
                    cx.notify();
                })
                .ok();
        })
        .detach();

        Self {
            connection: ConnectionState::Connecting { endpoint },
            editor,
            query: QueryState::Idle,
        }
    }

    fn run_query(&mut self, _: &RunQuery, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.query, QueryState::Running) {
            return;
        }

        let connection = match &self.connection {
            ConnectionState::Connected { connection, .. } => connection.clone(),
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
                        Ok(result) => QueryState::Complete(result),
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
            ConnectionState::Connected { endpoint, .. } => {
                (format!("Connected to {endpoint}"), t.success)
            }
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
            QueryState::Complete(result) => {
                let mut lines = Vec::new();
                if !result.columns.is_empty() {
                    lines.push(
                        result
                            .columns
                            .iter()
                            .map(|column| column.name.as_str())
                            .collect::<Vec<_>>()
                            .join("  |  "),
                    );
                }
                lines.extend(result.rows.iter().take(RESULT_PREVIEW_ROWS).map(|row| {
                    row.iter()
                        .map(|cell| cell.as_deref().unwrap_or("NULL"))
                        .collect::<Vec<_>>()
                        .join("  |  ")
                }));
                if result.rows.len() > RESULT_PREVIEW_ROWS {
                    lines.push(format!(
                        "Showing {RESULT_PREVIEW_ROWS} of {} rows.",
                        result.rows.len()
                    ));
                } else if result.rows.is_empty() {
                    lines.push(match result.rows_affected {
                        Some(rows) => format!("Query completed. Server row count: {rows}."),
                        None => "Query completed.".into(),
                    });
                }
                lines.push(format!(
                    "{} row(s) · {} bytes · {:.1?}",
                    result.rows.len(),
                    result.bytes,
                    result.elapsed
                ));
                lines
            }
        };

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
                    .p(px(layout::SPACE_LG))
                    .font_family(gpui_component::Theme::global(cx).mono_font_family.clone())
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
                    .overflow_y_scrollbar()
                    .border_t_1()
                    .border_color(t.border)
                    .p(px(layout::SPACE_LG))
                    .font_family(gpui_component::Theme::global(cx).mono_font_family.clone())
                    .text_color(if matches!(self.query, QueryState::Failed(_)) {
                        t.danger
                    } else {
                        t.text
                    })
                    .children(result_lines.into_iter().map(|line| {
                        div()
                            .w_full()
                            .py(px(layout::SPACE_XS))
                            .child(line)
                    })),
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
                    .child(status),
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
        ("PGPORT", &port),
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
        .expect("checked above")
        .parse()
        .map_err(|_| "PGPORT is not a valid port.".to_string())?;

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
