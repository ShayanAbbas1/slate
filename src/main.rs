mod db;
mod sql;

mod theme;

use gpui::{
    App, AppContext, Application, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Window, WindowOptions, div, px,
};
use gpui_component::{Root, input::{Input, InputState}};

use db::{Connection, ConnectionConfig};
use theme::{Appearance, Theme, layout, theme};

enum ConnectionState {
    NotConfigured,
    Connecting {
        endpoint: String,
    },
    Connected {
        _connection: Connection,
        endpoint: String,
    },
    Failed(String),
}

struct Workspace {
    connection: ConnectionState,
    editor: Entity<InputState>,
}

impl Workspace {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder("Write SQL…")
        });

        let config = match connection_config_from_environment() {
            Ok(Some(config)) => config,
            Ok(None) => {
                return Self {
                    connection: ConnectionState::NotConfigured,
                    editor,
                };
            }
            Err(message) => {
                return Self {
                    connection: ConnectionState::Failed(message),
                    editor,
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
                            _connection: connection,
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
        }
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

        div()
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
