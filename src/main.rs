mod db;
mod explorer;
mod result_grid;
mod sql;
mod store;

mod icons;
mod theme;

use std::{borrow::Cow, collections::HashMap, sync::Arc};

use gpui::{
    Action, AnyElement, App, AppContext, Application, ClickEvent, Context, Entity,
    EntityInputHandler, Focusable, FontWeight, InteractiveElement, IntoElement, KeyBinding,
    Keystroke, ParentElement, Render, StatefulInteractiveElement, Styled, TitlebarOptions, Window,
    WindowOptions, actions, div, point, prelude::FluentBuilder, px,
};
use serde::Deserialize;
use gpui_component::{
    InteractiveElementExt, Root, Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    kbd::Kbd,
    list::ListItem,
    resizable::{h_resizable, resizable_panel, v_resizable},
    table::{Table, TableDelegate, TableState},
    tree::{TreeState, tree as render_tree},
};

use db::{
    Catalog, Connection, ConnectionConfig, DbError, RelationKind, Routine, RoutineKind, Structure,
};
use explorer::{ExplorerLeaf, ExplorerTarget, ObjectKind, preview_sql, tree as build_explorer_tree};
use icons::{Icons, icon};
use result_grid::ResultGrid;
use sql::{Buffer, SortKey};
use theme::{Theme, layout, theme};

/// A header click. The column is the one in the grid; which statement it
/// belongs to is whatever surface is in front, because that is the grid the
/// click came from.
#[derive(Clone, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = slate, no_json)]
struct SortColumn {
    column: usize,
}

actions!(
    slate,
    [
        RunQuery,
        ShowEditor,
        CycleTheme,
        SaveQuery,
        NewQuery,
        NextProfile,
        PreviousProfile,
        NewConnection,
        ZoomEditorIn,
        ZoomEditorOut,
        ResetEditorZoom,
    ]
);

const EDITOR_FONT_SIZE_DEFAULT: f32 = 14.0;
const EDITOR_FONT_SIZE_MIN: f32 = 11.0;
const EDITOR_FONT_SIZE_MAX: f32 = 24.0;
const EDITOR_FONT_SIZE_STEP: f32 = 1.0;

/// The platform's window buttons, which Slate positions but does not draw.
const TRAFFIC_LIGHT_DIAMETER: f32 = 14.0;

/// A connection and everything it owns.
///
/// The editor, results, explorer and query state live here rather than on
/// `Workspace` deliberately (spec §3.1). A profile is replaced wholesale when
/// the connection changes, so a buffer written against one database cannot be
/// retargeted at another — it does not exist outside its profile.
struct Profile {
    id: String,
    name: String,
    config: ConnectionConfig,
    generation: u64,
    state: ProfileState,
    catalog: CatalogState,
    session: Session,
}

impl Profile {
    fn connection(&self) -> Option<Connection> {
        match &self.state {
            ProfileState::Connected(connection) => Some(connection.clone()),
            _ => None,
        }
    }

    fn stored(&self) -> store::StoredProfile {
        // Tabs read back from disk that the catalog has not named yet are still
        // the truth about this profile: writing the live list instead would
        // drop every restored object the first time anything else is saved.
        //
        // A transient tab is deliberately not written: it was opened for a look,
        // and coming back to a window full of things nobody chose to keep is
        // the whole reason preview tabs exist.
        let open_objects = if self.session.pending_objects.is_empty() {
            let active = self.session.active;
            self.session
                .objects
                .iter()
                .filter(|tab| !tab.transient)
                .map(|tab| store::StoredObject {
                    active: active == Tab::Object(tab.id),
                    ..tab.stored()
                })
                .collect()
        } else {
            self.session.pending_objects.clone()
        };

        store::StoredProfile {
            id: self.id.clone(),
            name: self.name.clone(),
            host: self.config.host.clone(),
            port: self.config.port,
            database: self.config.database.clone(),
            user: self.config.user.clone(),
            open_query: self.session.open_query.clone(),
            open_objects,
        }
    }
}

enum ProfileState {
    Idle,
    Connecting,
    Connected(Connection),
    Failed(String),
}

/// The per-profile view state.
///
/// Separate from `Profile` only because GPUI entities need a `&mut Window` to
/// create, and the connection resolves on a task that has none — so this is
/// built before the spawn and moved in once the connection opens.
struct Session {
    editor: Entity<InputState>,
    /// The editor's own grid. Every object tab owns another, so switching tabs
    /// cannot leave one surface's rows sitting under another's heading.
    results: Entity<TableState<ResultGrid>>,
    query: QueryState,
    objects: Vec<ObjectTab>,
    active: Tab,
    next_object_id: u64,
    /// Object tabs read back from disk, held until the catalog can name them.
    pending_objects: Vec<store::StoredObject>,
    explorer_filter: Entity<InputState>,
    explorer_tree: Entity<TreeState>,
    explorer_leaves: Arc<HashMap<String, ExplorerLeaf>>,
    /// `cmd+enter` reaches the workspace only through the focused element's
    /// dispatch path, so an unfocused editor makes the primary keystroke dead.
    editor_needs_focus: bool,
    /// The same hazard for the name field: an unfocused input asks for a name
    /// nobody can type into.
    save_name_needs_focus: bool,
    open_query: Option<String>,
    saved_queries: Vec<String>,
    save_name: Entity<InputState>,
    naming: bool,
    pending_delete: Option<String>,
    notice: Option<String>,
    editor_font_size: f32,
}

impl Session {
    fn new(
        id: String,
        open_query: Option<String>,
        pending_objects: Vec<store::StoredObject>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let explorer_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter database objects…"));
        cx.subscribe(&explorer_filter, {
            let id = id.clone();
            move |workspace, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    workspace.refresh_explorer(&id, cx);
                }
            }
        })
        .detach();

        let save_name = cx.new(|cx| InputState::new(window, cx).placeholder("Query name"));
        // Subscribed with the window, because confirming a save can swap the
        // editor's buffer and that cannot be done without one.
        cx.subscribe_in(
            &save_name,
            window,
            |workspace, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    workspace.confirm_save(window, cx);
                }
            },
        )
        .detach();

        let saved_queries = store::saved_queries(&id);
        let open_query = open_query.filter(|name| saved_queries.contains(name));
        let sql = match &open_query {
            Some(name) => store::read_query(&id, name),
            None => store::read_scratch(&id),
        }
        .unwrap_or_default();

        Self {
            editor: cx.new(|cx| {
                InputState::new(window, cx)
                    .code_editor("sql")
                    .soft_wrap(false)
                    .placeholder("Write SQL…")
                    .default_value(sql)
            }),
            results: result_grid(window, cx),
            query: QueryState::Idle,
            objects: Vec::new(),
            active: Tab::Query,
            next_object_id: 0,
            pending_objects,
            explorer_filter,
            explorer_tree: cx.new(|cx| TreeState::new(cx)),
            explorer_leaves: Arc::new(HashMap::new()),
            editor_needs_focus: true,
            save_name_needs_focus: false,
            open_query,
            saved_queries,
            save_name,
            naming: false,
            pending_delete: None,
            notice: None,
            editor_font_size: EDITOR_FONT_SIZE_DEFAULT,
        }
    }

    fn active_object(&self) -> Option<&ObjectTab> {
        match self.active {
            Tab::Object(id) => self.objects.iter().find(|tab| tab.id == id),
            Tab::Query => None,
        }
    }

    /// The query state behind the visible surface, or `None` for a surface that
    /// runs nothing — a routine is read, never executed by being opened.
    fn active_query(&self) -> Option<&QueryState> {
        match self.active_object() {
            None => Some(&self.query),
            Some(tab) => match &tab.body {
                ObjectBody::Relation { query, .. } => Some(query),
                ObjectBody::Routine(_) => None,
            },
        }
    }

    /// The buffer a run reads from, which only the query tab has. An object tab
    /// shows an object: there is no SQL in front of the user to run.
    fn editor(&self, tab: Tab) -> Option<Entity<InputState>> {
        match tab {
            Tab::Query => Some(self.editor.clone()),
            Tab::Object(_) => None,
        }
    }

    /// Where a run's state and rows belong. Returning both together is what
    /// keeps a result from landing in one tab's grid with another tab's status.
    fn slot(&mut self, tab: Tab) -> Option<(&mut QueryState, Entity<TableState<ResultGrid>>)> {
        match tab {
            Tab::Query => Some((&mut self.query, self.results.clone())),
            Tab::Object(id) => {
                match &mut self.objects.iter_mut().find(|tab| tab.id == id)?.body {
                    ObjectBody::Relation {
                        query, results, ..
                    } => Some((query, results.clone())),
                    ObjectBody::Routine(_) => None,
                }
            }
        }
    }

    fn promote(&mut self, id: u64) -> bool {
        match self.objects.iter_mut().find(|tab| tab.id == id) {
            Some(tab) if tab.transient => {
                tab.transient = false;
                true
            }
            _ => false,
        }
    }

}

/// What takes focus when a surface comes to the front. A buffer and a grid are
/// both focusable and neither is the other's type.
enum Focus {
    Buffer(Entity<InputState>),
    Grid(Entity<TableState<ResultGrid>>),
}

enum CatalogState {
    Loading,
    Loaded(Catalog),
    Failed(String),
}

/// Which surface the main pane is showing, and what a run targets. Object tabs
/// are addressed by id rather than by index, so closing one cannot land an
/// in-flight result in its neighbour's grid.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Query,
    Object(u64),
}

/// An opened database object. It stays in the tab strip until it is closed, so
/// coming back to a table does not mean finding it in the explorer again.
struct ObjectTab {
    id: u64,
    schema: String,
    /// A relation's name, or a routine's name with its argument types — which
    /// is the only thing that tells two overloads of one function apart.
    name: String,
    kind: ObjectKind,
    /// Opened for a look rather than to be kept. One click gets a transient
    /// tab, the next one replaces it, and only a deliberate gesture — a double
    /// click, or typing in its buffer — makes it stay.
    transient: bool,
    body: ObjectBody,
}

impl ObjectTab {
    fn stored(&self) -> store::StoredObject {
        store::StoredObject {
            schema: self.schema.clone(),
            name: self.name.clone(),
            routine: matches!(self.kind, ObjectKind::Routine(_)),
            active: false,
        }
    }
}

/// What the explorer -- or a session read back from disk -- hands over to open
/// a tab. A routine arrives whole, because its body is already in the catalog.
enum OpenedObject {
    Relation {
        schema: String,
        name: String,
        kind: RelationKind,
    },
    Routine {
        schema: String,
        routine: Routine,
    },
}

impl OpenedObject {
    fn schema(&self) -> &str {
        match self {
            Self::Relation { schema, .. } | Self::Routine { schema, .. } => schema,
        }
    }

    fn name(&self) -> String {
        match self {
            Self::Relation { name, .. } => name.clone(),
            Self::Routine { routine, .. } => routine_name(routine),
        }
    }

    fn kind(&self) -> ObjectKind {
        match self {
            Self::Relation { kind, .. } => ObjectKind::Relation(*kind),
            Self::Routine { routine, .. } => ObjectKind::Routine(routine.kind),
        }
    }

    fn resolve(catalog: &Catalog, stored: &store::StoredObject) -> Option<Self> {
        let schema = catalog
            .schemas
            .iter()
            .find(|schema| schema.name == stored.schema)?;
        if stored.routine {
            let routine = schema
                .routines
                .iter()
                .find(|routine| routine_name(routine) == stored.name)?;
            Some(Self::Routine {
                schema: schema.name.clone(),
                routine: routine.clone(),
            })
        } else {
            let relation = schema
                .relations
                .iter()
                .find(|relation| relation.name == stored.name)?;
            Some(Self::Relation {
                schema: schema.name.clone(),
                name: relation.name.clone(),
                kind: relation.kind,
            })
        }
    }
}

/// A routine's name carries its argument types, because a schema can hold
/// several routines with the same name and nothing else to tell them apart.
fn routine_name(routine: &Routine) -> String {
    format!("{}({})", routine.name, routine.identity_arguments)
}

enum ObjectBody {
    /// An opened relation: its rows, full height, with the relation's
    /// definition behind the Structure toggle (spec §3.2).
    ///
    /// No editor. A generated `SELECT` shown above the grid read as a query the
    /// user had written and invited edits to a buffer that then stopped being a
    /// view of the relation at all. The SQL Slate runs here is its own, and the
    /// only thing the user changes about it is the sort.
    Relation {
        showing_structure: bool,
        structure: StructureState,
        results: Entity<TableState<ResultGrid>>,
        query: QueryState,
        /// The `ORDER BY` the header clicks have built up. Slate owns this
        /// statement, so sorting regenerates it rather than editing text.
        sort: Vec<SortKey>,
    },
    Routine(Routine),
}

enum StructureState {
    Loading,
    Loaded(Structure),
    Failed(String),
}

fn result_grid(
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<TableState<ResultGrid>> {
    cx.new(|cx| {
        TableState::new(ResultGrid::empty(), window, cx)
            // Sorting is the grid's own, over the rows it already holds. It
            // never re-runs the statement, so the rows on screen stay the one
            // snapshot the server sent.
            .sortable(true)
            .col_movable(false)
            .col_resizable(true)
            .row_selectable(true)
            .col_selectable(true)
    })
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
    profiles: Vec<Profile>,
    active: usize,
    form: Option<ConnectionForm>,
    switcher_open: bool,
    pending_removal: Option<String>,
    next_generation: u64,
}

impl Workspace {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut workspace = Self {
            profiles: Vec::new(),
            active: 0,
            form: None,
            switcher_open: false,
            pending_removal: None,
            next_generation: 0,
        };

        for stored in store::load_profiles() {
            workspace.restore_profile(stored, window, cx);
        }

        match connection_config_from_environment() {
            Ok(Some(config)) => {
                let existing = workspace.profiles.iter().position(|profile| {
                    profile.config.endpoint() == config.endpoint()
                        && profile.config.user == config.user
                });
                workspace.active = match existing {
                    Some(index) => index,
                    None => {
                        let name = config.database.clone();
                        workspace.create_profile(name, config, window, cx)
                    }
                };
            }
            Ok(None) => {}
            Err(message) => {
                let mut form = ConnectionForm::new(None, window, cx);
                form.error = Some(message);
                workspace.form = Some(form);
            }
        }

        if workspace.profiles.is_empty() && workspace.form.is_none() {
            workspace.form = Some(ConnectionForm::new(None, window, cx));
        }

        // Buffers are otherwise written only when one is swapped for another,
        // so without this everything typed since the last swap dies with the
        // process -- which is the one moment a person expects it to be kept.
        cx.on_app_quit(|workspace: &mut Self, cx: &mut Context<Self>| {
            workspace.persist_buffers(cx);
            async {}
        })
        .detach();
        // Closing the window does not quit the application, so without this the
        // red button is a way to lose everything typed since the last swap.
        cx.on_release(|workspace, cx| workspace.persist_buffers(cx))
            .detach();

        workspace.connect_active(cx);
        workspace
    }

    fn profile(&self) -> Option<&Profile> {
        self.profiles.get(self.active)
    }

    fn profile_mut(&mut self) -> Option<&mut Profile> {
        self.profiles.get_mut(self.active)
    }

    fn issued_to(&mut self, id: &str, generation: u64) -> Option<&mut Profile> {
        self.profiles
            .iter_mut()
            .find(|profile| profile.id == id && profile.generation == generation)
    }

    fn note(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut() {
            profile.session.notice = Some(message);
        } else if let Some(form) = &mut self.form {
            form.error = Some(message);
        }
        cx.notify();
    }

    fn zoom_editor_in(&mut self, _: &ZoomEditorIn, _: &mut Window, cx: &mut Context<Self>) {
        self.adjust_editor_zoom(EDITOR_FONT_SIZE_STEP, cx);
    }

    fn zoom_editor_out(&mut self, _: &ZoomEditorOut, _: &mut Window, cx: &mut Context<Self>) {
        self.adjust_editor_zoom(-EDITOR_FONT_SIZE_STEP, cx);
    }

    fn reset_editor_zoom(&mut self, _: &ResetEditorZoom, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut() {
            profile.session.editor_font_size = EDITOR_FONT_SIZE_DEFAULT;
            cx.notify();
        }
    }

    fn adjust_editor_zoom(&mut self, delta: f32, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut() {
            profile.session.editor_font_size =
                adjusted_editor_font_size(profile.session.editor_font_size, delta);
            cx.notify();
        }
    }

    fn remember_profiles(&mut self, cx: &mut Context<Self>) {
        let profiles = self
            .profiles
            .iter()
            .map(Profile::stored)
            .collect::<Vec<_>>();
        if let Err(message) = store::save_profiles(&profiles) {
            self.note(message, cx);
        }
    }

    fn restore_profile(
        &mut self,
        stored: store::StoredProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let config = ConnectionConfig {
            host: stored.host,
            port: stored.port,
            database: stored.database,
            user: stored.user,
            password: String::new(),
        };
        let session = Session::new(
            stored.id.clone(),
            stored.open_query,
            stored.open_objects,
            window,
            cx,
        );
        self.profiles.push(Profile {
            id: stored.id,
            name: stored.name,
            config,
            generation: 0,
            state: ProfileState::Idle,
            catalog: CatalogState::Loading,
            session,
        });
    }

    fn create_profile(
        &mut self,
        name: String,
        config: ConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let existing = self
            .profiles
            .iter()
            .map(|profile| profile.id.clone())
            .collect::<Vec<_>>();
        let id = store::profile_id(&name, &existing);
        let session = Session::new(id.clone(), None, Vec::new(), window, cx);
        let password = config.password.clone();
        self.profiles.push(Profile {
            id: id.clone(),
            name,
            config,
            generation: 0,
            state: ProfileState::Idle,
            catalog: CatalogState::Loading,
            session,
        });
        if let Err(message) = store::set_password(&id, &password) {
            self.note(message, cx);
        }
        self.remember_profiles(cx);
        self.profiles.len() - 1
    }

    fn apply_connection_url(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &self.form else {
            return;
        };
        let url = form.url.read(cx).value();
        let config = match ConnectionConfig::from_url(url.trim()) {
            Ok(config) => config,
            Err(error) => {
                if let Some(form) = &mut self.form {
                    form.error = Some(error);
                }
                cx.notify();
                return;
            }
        };

        for (input, value) in [
            (&form.name, config.database.clone()),
            (&form.host, config.host),
            (
                &form.port,
                config.port.map(|port| port.to_string()).unwrap_or_default(),
            ),
            (&form.database, config.database),
            (&form.user, config.user),
            (&form.password, config.password),
        ] {
            let input = input.clone();
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        if let Some(form) = &mut self.form {
            form.error = None;
        }
        cx.notify();
    }

    fn connect(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        let (name, config) = match form.config(cx) {
            Ok(profile) => profile,
            Err(error) => {
                if let Some(form) = &mut self.form {
                    form.error = Some(error);
                }
                cx.notify();
                return;
            }
        };

        self.form = None;
        let index = self.create_profile(name, config, window, cx);
        self.activate(index, cx);
    }

    fn open_connection_form(
        &mut self,
        _: &NewConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.form = Some(ConnectionForm::new(None, window, cx));
        self.switcher_open = false;
        cx.notify();
    }

    fn connect_active(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.profile().map(|profile| &profile.state),
            Some(ProfileState::Idle | ProfileState::Failed(_))
        ) {
            self.begin_connect(self.active, cx);
        }
    }

    fn begin_connect(&mut self, index: usize, cx: &mut Context<Self>) {
        self.next_generation += 1;
        let generation = self.next_generation;
        let Some(profile) = self.profiles.get_mut(index) else {
            return;
        };
        profile.generation = generation;
        profile.state = ProfileState::Connecting;
        profile.catalog = CatalogState::Loading;

        let id = profile.id.clone();
        let mut config = profile.config.clone();
        cx.notify();

        let connection_task = cx.background_executor().spawn({
            let id = id.clone();
            async move {
                if config.password.is_empty() {
                    config.password = store::password(&id).unwrap_or_default();
                }
                Connection::open(config)
            }
        });

        cx.spawn(async move |workspace, cx| {
            let result = connection_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    let Some(profile) = workspace.issued_to(&id, generation) else {
                        return;
                    };
                    profile.state = match result {
                        Ok(connection) => ProfileState::Connected(connection),
                        Err(error) => ProfileState::Failed(error.message),
                    };
                    workspace.load_catalog(&id, generation, cx);
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn load_catalog(&mut self, id: &str, generation: u64, cx: &mut Context<Self>) {
        let Some(connection) = self
            .issued_to(id, generation)
            .and_then(|profile| profile.connection())
        else {
            return;
        };
        let catalog_task = cx
            .background_executor()
            .spawn(async move { connection.catalog() });

        let id = id.to_string();
        cx.spawn(async move |workspace, cx| {
            let result = catalog_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    let Some(profile) = workspace.issued_to(&id, generation) else {
                        return;
                    };
                    profile.catalog = match result {
                        Ok(catalog) => CatalogState::Loaded(catalog),
                        Err(error) => CatalogState::Failed(error.message),
                    };
                    workspace.refresh_explorer(&id, cx);
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn refresh_explorer(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter().find(|profile| profile.id == id) else {
            return;
        };
        let filter = profile.session.explorer_filter.read(cx).value();
        let explorer = match &profile.catalog {
            CatalogState::Loaded(catalog) => build_explorer_tree(catalog, &filter),
            _ => explorer::ExplorerTree {
                items: Vec::new(),
                leaves: HashMap::new(),
            },
        };
        let tree = profile.session.explorer_tree.clone();

        if let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) {
            profile.session.explorer_leaves = Arc::new(explorer.leaves);
        }
        tree.update(cx, |tree, cx| tree.set_items(explorer.items, cx));
    }

    fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.profiles.len() {
            return;
        }
        if let Err(message) = self.persist_buffer(cx) {
            self.note(message, cx);
        }
        self.active = index;
        self.form = None;
        self.switcher_open = false;
        self.pending_removal = None;
        if let Some(profile) = self.profile_mut() {
            profile.session.editor_needs_focus = true;
            profile.session.pending_delete = None;
        }
        self.connect_active(cx);
        cx.notify();
    }

    fn cycle_profile(&mut self, step: isize, cx: &mut Context<Self>) {
        if self.profiles.len() < 2 || self.form.is_some() {
            return;
        }
        let count = self.profiles.len() as isize;
        let index = (self.active as isize + step).rem_euclid(count) as usize;
        self.activate(index, cx);
    }

    fn next_profile(&mut self, _: &NextProfile, _: &mut Window, cx: &mut Context<Self>) {
        self.cycle_profile(1, cx);
    }

    fn previous_profile(&mut self, _: &PreviousProfile, _: &mut Window, cx: &mut Context<Self>) {
        self.cycle_profile(-1, cx);
    }

    fn remove_profile(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.get(index) else {
            return;
        };
        let id = profile.id.clone();
        let name = profile.name.clone();
        if self.pending_removal.as_deref() != Some(&id) {
            self.pending_removal = Some(id);
            cx.notify();
            return;
        }
        if index == self.active
            && let Err(message) = self.persist_buffer(cx)
        {
            self.note(message, cx);
            return;
        }

        self.profiles.remove(index);
        store::delete_password(&id);
        self.pending_removal = None;
        self.active = self.active.min(self.profiles.len().saturating_sub(1));
        self.remember_profiles(cx);
        if self.profiles.is_empty() {
            self.form = Some(ConnectionForm::new(None, window, cx));
        } else {
            self.connect_active(cx);
            self.note(format!("Removed {name}."), cx);
        }
        cx.notify();
    }

    fn open_explorer_target(
        &mut self,
        target: ExplorerTarget,
        transient: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(catalog) = self.catalog() else {
            return;
        };

        let opened = match target {
            ExplorerTarget::Relation {
                schema_index,
                relation_index,
            } => catalog.schemas.get(schema_index).and_then(|schema| {
                let relation = schema.relations.get(relation_index)?;
                Some(OpenedObject::Relation {
                    schema: schema.name.clone(),
                    name: relation.name.clone(),
                    kind: relation.kind,
                })
            }),
            ExplorerTarget::Routine {
                schema_index,
                routine_index,
            } => catalog.schemas.get(schema_index).and_then(|schema| {
                let routine = schema.routines.get(routine_index)?;
                Some(OpenedObject::Routine {
                    schema: schema.name.clone(),
                    routine: routine.clone(),
                })
            }),
        };

        if let Some(opened) = opened
            && let Some(id) = self.open_object(opened, transient, window, cx)
        {
            self.activate_tab(Tab::Object(id), cx);
            self.remember_profiles(cx);
        }
    }

    /// Give an object a tab, reusing the one it already has. Opening does not
    /// show it — the caller decides that, so restoring a session can rebuild
    /// six tabs without running six queries.
    ///
    /// A transient tab takes the place of the last transient one, the way a
    /// preview tab works in an editor: browsing the tree leaves one tab behind,
    /// not thirty.
    fn open_object(
        &mut self,
        opened: OpenedObject,
        transient: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let (schema, name, kind) = (opened.schema().to_string(), opened.name(), opened.kind());
        let profile = self.profile_mut()?;
        let existing = profile
            .session
            .objects
            .iter()
            .find(|tab| tab.schema == schema && tab.name == name)
            .map(|tab| tab.id);

        if let Some(id) = existing {
            if !transient {
                profile.session.promote(id);
            }
            return Some(id);
        }

        if transient {
            let replaced = profile
                .session
                .objects
                .iter()
                .filter(|tab| tab.transient)
                .map(|tab| tab.id)
                .collect::<Vec<_>>();
            profile
                .session
                .objects
                .retain(|tab| !replaced.contains(&tab.id));
        }

        let id = profile.session.next_object_id;
        profile.session.next_object_id += 1;
        let body = match opened {
            OpenedObject::Routine { routine, .. } => ObjectBody::Routine(routine),
            OpenedObject::Relation { .. } => ObjectBody::Relation {
                showing_structure: false,
                structure: StructureState::Loading,
                results: result_grid(window, cx),
                query: QueryState::Idle,
                sort: Vec::new(),
            },
        };
        self.profile_mut()?.session.objects.push(ObjectTab {
            id,
            schema,
            name,
            kind,
            transient,
            body,
        });
        Some(id)
    }

    /// Run a relation's `SELECT` and load its structure, once. Reaching a tab
    /// again must not re-query — the rows it already holds are why the tab is
    /// worth keeping open — but a failed run is not a result, so that one
    /// is allowed to be tried again.
    fn load_relation(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(tab) = self
            .profile()
            .and_then(|profile| profile.session.objects.iter().find(|tab| tab.id == id))
        else {
            return;
        };
        let ObjectBody::Relation { query, sort, .. } = &tab.body else {
            return;
        };
        if !matches!(query, QueryState::Idle | QueryState::Failed(_)) {
            return;
        }

        let (schema, relation) = (tab.schema.clone(), tab.name.clone());
        let sql = relation_sql(&schema, &relation, sort);
        self.load_structure(id, schema, relation, cx);
        self.execute_sql(sql, Tab::Object(id), cx);
    }

    /// The statement behind a relation's tab: Slate's own preview, carrying the
    /// sort the header clicks asked for. Regenerated rather than edited, so the
    /// row limit and the quoting cannot drift out of one place.
    fn relation_sort(&mut self, id: u64, column: usize, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(tab) = profile.session.objects.iter_mut().find(|tab| tab.id == id) else {
            return;
        };
        let (schema, relation) = (tab.schema.clone(), tab.name.clone());
        let ObjectBody::Relation {
            sort,
            results,
            query,
            ..
        } = &mut tab.body
        else {
            return;
        };

        let Some(expression) = sort_expression(results.read(cx).delegate().columns(), column) else {
            return;
        };
        cycle(sort, &expression);
        let sql = relation_sql(&schema, &relation, sort);
        // A preview only re-queries when it is asked to, and this is the ask.
        *query = QueryState::Idle;
        self.execute_sql(sql, Tab::Object(id), cx);
    }

    fn load_structure(
        &mut self,
        id: u64,
        schema: String,
        relation: String,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(connection) = profile.connection() else {
            return;
        };
        let profile_id = profile.id.clone();
        let generation = profile.generation;
        let structure_task = cx
            .background_executor()
            .spawn(async move { connection.structure(&schema, &relation) });

        cx.spawn(async move |workspace, cx| {
            let result = structure_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    let Some(profile) = workspace.issued_to(&profile_id, generation) else {
                        return;
                    };
                    // Addressed by tab, so a second object opened while this was
                    // in flight cannot end up wearing this one's columns.
                    let Some(tab) = profile.session.objects.iter_mut().find(|tab| tab.id == id)
                    else {
                        return;
                    };
                    if let ObjectBody::Relation { structure, .. } = &mut tab.body {
                        *structure = match result {
                            Ok(loaded) => StructureState::Loaded(loaded),
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
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Tab::Object(id) = profile.session.active else {
            return;
        };
        if let Some(tab) = profile.session.objects.iter_mut().find(|tab| tab.id == id)
            && let ObjectBody::Relation { showing_structure: showing, .. } = &mut tab.body
        {
            *showing = showing_structure;
            cx.notify();
        }
    }

    fn activate_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut() {
            profile.session.active = tab;
            profile.session.pending_delete = None;
            // A half-finished name belongs to the buffer it was opened over.
            // Left standing it would name a different one, and it holds the
            // focus the new surface needs.
            profile.session.naming = false;
            profile.session.editor_needs_focus = true;
        }
        if let Tab::Object(id) = tab {
            self.load_relation(id, cx);
        }
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Keep a tab that was opened for a look.
    fn keep_object(&mut self, id: u64, cx: &mut Context<Self>) {
        if self
            .profile_mut()
            .is_some_and(|profile| profile.session.promote(id))
        {
            self.remember_profiles(cx);
            cx.notify();
        }
    }

    fn close_object(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut() {
            profile.session.objects.retain(|tab| tab.id != id);
            if profile.session.active == Tab::Object(id) {
                profile.session.active = Tab::Query;
                profile.session.editor_needs_focus = true;
            }
        }
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Turn the object tabs read back from disk into live ones, now that the
    /// catalog can say what they hold. Anything the database no longer has
    /// simply does not come back.
    fn restore_objects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        if profile.session.pending_objects.is_empty() {
            return;
        }
        let CatalogState::Loaded(catalog) = &profile.catalog else {
            return;
        };

        let opened = profile
            .session
            .pending_objects
            .iter()
            .filter_map(|stored| {
                Some((OpenedObject::resolve(catalog, stored)?, stored.active))
            })
            .collect::<Vec<_>>();

        if let Some(profile) = self.profile_mut() {
            profile.session.pending_objects.clear();
        }
        let mut restored_active = None;
        for (opened, active) in opened {
            let id = self.open_object(opened, false, window, cx);
            if active {
                restored_active = id;
            }
        }
        match restored_active {
            Some(id) => self.activate_tab(Tab::Object(id), cx),
            // Nothing to activate, but the pending list was drained, so what is
            // on disk has to be rewritten from the tabs that actually resolved.
            None => self.remember_profiles(cx),
        }
    }

    fn catalog(&self) -> Option<&Catalog> {
        match self.profile().map(|profile| &profile.catalog) {
            Some(CatalogState::Loaded(catalog)) => Some(catalog),
            _ => None,
        }
    }

    /// Swap to the next registered theme. Every colour Slate paints is read
    /// from the global at render time, so repainting is the whole change — and
    /// side-by-side comparison is the only honest way to pick between palettes.
    fn cycle_theme(&mut self, _: &CycleTheme, _: &mut Window, cx: &mut Context<Self>) {
        let next = theme(cx).next();
        next.apply_to_components(cx);
        cx.set_global(next);
        // The titlebar deliberately no longer names the theme -- permanent
        // chrome should not narrate a setting -- so the switch itself says
        // where it landed.
        if self.profile().is_some() {
            self.note(format!("Theme: {}", next.name), cx);
        }
        cx.refresh_windows();
    }

    /// Return to the editor, backing out of whatever is in front of it.
    fn show_editor(&mut self, _: &ShowEditor, _: &mut Window, cx: &mut Context<Self>) {
        if self.form.is_some() && !self.profiles.is_empty() {
            self.form = None;
            cx.notify();
            return;
        }
        let Some(profile) = self.profile_mut() else {
            return;
        };
        if profile.session.naming {
            profile.session.naming = false;
            profile.session.editor_needs_focus = true;
            cx.notify();
            return;
        }
        if matches!(profile.session.active, Tab::Query) {
            return;
        }
        profile.session.active = Tab::Query;
        profile.session.editor_needs_focus = true;
        self.remember_profiles(cx);
        cx.notify();
    }

    fn run_query(&mut self, _: &RunQuery, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let tab = profile.session.active;
        // An object tab has no buffer of its own: running it again is a refresh
        // of the rows Slate fetched, which is the only thing there is to run.
        if let Tab::Object(id) = tab {
            self.refresh_relation(id, cx);
            return;
        }
        let Some(editor) = profile.session.editor(tab) else {
            return;
        };

        let Some(sql) = self.sql_to_run(&editor, window, cx) else {
            if let Some(profile) = self.profile_mut()
                && let Some((state, _)) = profile.session.slot(tab)
            {
                *state = QueryState::Failed(DbError {
                    message: "There is no statement to run.".into(),
                    position: None,
                });
            }
            cx.notify();
            return;
        };

        self.execute_sql(sql, tab, cx);
    }

    /// Run a relation's statement again. The rows are a snapshot, and this is
    /// the only way to ask for a newer one.
    fn refresh_relation(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(tab) = profile.session.objects.iter_mut().find(|tab| tab.id == id) else {
            return;
        };
        let (schema, relation) = (tab.schema.clone(), tab.name.clone());
        let ObjectBody::Relation { sort, query, .. } = &mut tab.body else {
            return;
        };
        let sql = relation_sql(&schema, &relation, sort);
        *query = QueryState::Idle;
        self.execute_sql(sql, Tab::Object(id), cx);
    }

    /// A column header was clicked: put that column into the statement's
    /// `ORDER BY` and run it again.
    ///
    /// The sort is the server's, not the grid's. Ordering the rows already
    /// fetched would sort one page of a table and call it sorted; asking the
    /// database means the top of the sort is the table's, not the page's.
    ///
    /// A click appends: a column that is not in the sort joins the end of it,
    /// one that is ascending turns around, and one that is descending drops
    /// out. Several columns therefore build up a compound sort by clicking.
    fn sort_column(&mut self, action: &SortColumn, window: &mut Window, cx: &mut Context<Self>) {
        let column = action.column;
        let Some(profile) = self.profile() else {
            return;
        };

        match profile.session.active {
            Tab::Object(id) => self.relation_sort(id, column, cx),
            Tab::Query => self.query_sort(column, window, cx),
        }
    }

    /// Sorting a query the user wrote: the `ORDER BY` goes into their statement,
    /// where they can see it, edit it and undo it. Slate changes SQL only when
    /// asked, and a header click is the ask (`AGENTS.md`, rule 1).
    fn query_sort(&mut self, column: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let editor = profile.session.editor.clone();
        let results = profile.session.results.clone();

        let Some(expression) = sort_expression(results.read(cx).delegate().columns(), column) else {
            return;
        };

        let (text, cursor) = {
            let editor = editor.read(cx);
            (editor.value().to_string(), editor.cursor())
        };
        let Some(range) = Buffer::parse(&text).statement_at(cursor) else {
            return;
        };
        let statement = &text[range.clone()];

        let Some(mut keys) = sql::order_by(statement) else {
            self.note(
                "Slate cannot add an ORDER BY to this statement without rewriting it.".into(),
                cx,
            );
            return;
        };
        cycle(&mut keys, &expression);
        let Some(sorted) = sql::with_order_by(statement, &keys) else {
            self.note("This statement cannot carry an ORDER BY.".into(), cx);
            return;
        };

        let mut replaced = text.clone();
        replaced.replace_range(range, &sorted);
        editor.update(cx, |editor, cx| editor.set_value(replaced, window, cx));
        self.execute_sql(sorted, Tab::Query, cx);
    }

    fn persist_buffer(&self, cx: &App) -> Result<(), String> {
        match self.profile() {
            Some(profile) => write_buffer(profile, cx),
            None => Ok(()),
        }
    }

    /// Every profile's buffer, for the one moment there is nowhere to report a
    /// failure to: the application is closing.
    fn persist_buffers(&self, cx: &App) {
        for profile in &self.profiles {
            let _ = write_buffer(profile, cx);
        }
    }

    fn save_query(&mut self, _: &SaveQuery, window: &mut Window, cx: &mut Context<Self>) {
        // A named query is written on every swap and on quit, so `cmd+s` on one
        // is a confirmation rather than a decision. Only a buffer with nowhere
        // to go has to ask for a name.
        if self.named() {
            match self.persist_buffer(cx) {
                Ok(()) => self.note("Saved query.".into(), cx),
                Err(message) => self.note(message, cx),
            }
            return;
        }
        self.ask_for_name(String::new(), window, cx);
    }

    /// Whether the visible buffer already has a name — which is what makes the
    /// difference between saving it and renaming it. A relation's tab never
    /// does: it holds SQL Slate wrote, not a file the user opened.
    fn named(&self) -> bool {
        self.profile().is_some_and(|profile| {
            matches!(profile.session.active, Tab::Query) && profile.session.open_query.is_some()
        })
    }

    fn rename_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self
            .profile()
            .and_then(|profile| profile.session.open_query.clone())
        else {
            return;
        };
        // Prefilled with its own name, unlike a save: the point of a rename is
        // to edit the name that is already there.
        self.ask_for_name(name, window, cx);
    }

    fn ask_for_name(&mut self, prefill: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        profile.session.naming = true;
        profile.session.save_name_needs_focus = true;
        profile.session.notice = None;
        let save_name = profile.session.save_name.clone();
        save_name.update(cx, |input, cx| input.set_value(prefill, window, cx));
        cx.notify();
    }

    fn confirm_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let name = profile.session.save_name.read(cx).value().trim().to_string();
        if let Err(message) = store::validate_query_name(&name) {
            self.note(message, cx);
            return;
        }
        let id = profile.id.clone();
        let tab = profile.session.active;
        let Some(editor) = profile.session.editor(tab) else {
            return;
        };
        // The name the buffer is leaving behind, if it had one. Present only
        // for a rename, since `cmd+s` on a named query never asks.
        let previous = match tab {
            Tab::Query => profile.session.open_query.clone(),
            Tab::Object(_) => None,
        };
        if previous.as_deref() != Some(name.as_str())
            && profile.session.saved_queries.contains(&name)
        {
            self.note(format!("A query named {name} already exists."), cx);
            return;
        }

        let sql = editor.read(cx).value().to_string();
        if let Err(message) = store::write_query(&id, &name, &sql) {
            self.note(message, cx);
            return;
        }
        // Written first, then the old name dropped: a failed delete leaves two
        // copies, which is recoverable, and the other order loses the query.
        if let Some(previous) = previous.filter(|previous| previous != &name)
            && let Err(message) = store::delete_query(&id, &previous)
        {
            self.note(message, cx);
        }

        if let Some(profile) = self.profile_mut() {
            profile.session.saved_queries = store::saved_queries(&id);
            profile.session.naming = false;
        }
        // Naming a relation's buffer is how it stops being a relation's buffer:
        // it leaves the object world entirely and becomes a saved query, which
        // is the only place a name means anything.
        match tab {
            Tab::Object(object) => {
                self.close_object(object, cx);
                self.open_saved_query(name.clone(), window, cx);
            }
            Tab::Query => {
                if let Some(profile) = self.profile_mut() {
                    profile.session.open_query = Some(name.clone());
                    profile.session.editor_needs_focus = true;
                }
            }
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.notice = Some(format!("Saved {name}."));
        }
        self.remember_profiles(cx);
        cx.notify();
    }

    fn new_query(&mut self, _: &NewQuery, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(message) = self.persist_buffer(cx) {
            self.note(message, cx);
            return;
        }
        let Some(profile) = self.profile_mut() else {
            return;
        };
        profile.session.open_query = None;
        profile.session.query = QueryState::Idle;
        profile.session.naming = false;
        profile.session.notice = None;
        profile
            .session
            .editor
            .update(cx, |editor, cx| editor.set_value("", window, cx));
        self.activate_tab(Tab::Query, cx);
    }

    fn open_saved_query(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .profile()
            .is_some_and(|profile| profile.session.open_query.as_deref() == Some(&name))
        {
            self.activate_tab(Tab::Query, cx);
            return;
        }
        if let Err(message) = self.persist_buffer(cx) {
            self.note(message, cx);
            return;
        }
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(sql) = store::read_query(&profile.id, &name) else {
            profile.session.saved_queries = store::saved_queries(&profile.id);
            profile.session.notice = Some(format!("{name} no longer exists."));
            cx.notify();
            return;
        };
        profile
            .session
            .editor
            .update(cx, |editor, cx| editor.set_value(sql, window, cx));
        profile.session.open_query = Some(name);
        profile.session.query = QueryState::Idle;
        profile.session.notice = None;
        self.activate_tab(Tab::Query, cx);
    }

    fn open_scratch_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .profile()
            .is_some_and(|profile| profile.session.open_query.is_none())
        {
            self.activate_tab(Tab::Query, cx);
            return;
        }
        if let Err(message) = self.persist_buffer(cx) {
            self.note(message, cx);
            return;
        }
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let sql = store::read_scratch(&profile.id).unwrap_or_default();
        profile
            .session
            .editor
            .update(cx, |editor, cx| editor.set_value(sql, window, cx));
        profile.session.open_query = None;
        profile.session.query = QueryState::Idle;
        profile.session.notice = None;
        self.activate_tab(Tab::Query, cx);
    }

    fn delete_saved_query(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        if profile.session.pending_delete.as_deref() != Some(&name) {
            profile.session.pending_delete = Some(name);
            cx.notify();
            return;
        }
        let id = profile.id.clone();
        if let Err(message) = store::delete_query(&id, &name) {
            self.note(message, cx);
            return;
        }
        if let Some(profile) = self.profile_mut() {
            if profile.session.open_query.as_deref() == Some(&name) {
                profile.session.open_query = None;
            }
            profile.session.saved_queries = store::saved_queries(&id);
            profile.session.pending_delete = None;
            profile.session.notice = Some(format!("Deleted {name}."));
        }
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Run `sql` against the active profile.
    ///
    /// There is deliberately no "not connected" branch: SQL is only reachable
    /// through a profile's own editor or explorer, so the absence of one is not
    /// a state the user can be shown an error about.
    fn execute_sql(&mut self, sql: String, tab: Tab, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let connection = profile.connection();
        let id = profile.id.clone();
        let generation = profile.generation;
        let Some((state, results)) = profile.session.slot(tab) else {
            return;
        };
        // Guarded here rather than in each caller: every path that runs SQL
        // routes through this one, and a caller that forgets would let two
        // results race into the grid with the older one landing last.
        if matches!(state, QueryState::Running) {
            return;
        }

        let Some(connection) = connection else {
            *state = QueryState::Failed(DbError {
                message: "The connection is not open.".into(),
                position: None,
            });
            cx.notify();
            return;
        };
        *state = QueryState::Running;

        // Rows from the previous statement must not sit under the one now on
        // screen -- a reader cannot tell stale rows from fresh ones.
        results.update(cx, |table, cx| {
            *table.delegate_mut() = ResultGrid::empty();
            // The inspector reads whatever row is selected, and a row index
            // means nothing once the rows behind it are gone.
            table.clear_selection(cx);
            table.refresh(cx);
        });
        cx.notify();

        // Read from the statement that is about to run, so the headers say what
        // the rows on screen are actually ordered by rather than what Slate
        // last intended to ask for.
        let keys = sql::order_by(&sql);
        let sortable = keys.is_some();
        let keys = keys.unwrap_or_default();
        let query_task = cx
            .background_executor()
            .spawn(async move { connection.query(&sql) });

        cx.spawn(async move |workspace, cx| {
            let result = query_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    let Some(profile) = workspace.issued_to(&id, generation) else {
                        return;
                    };
                    let Some((state, results)) = profile.session.slot(tab) else {
                        return;
                    };

                    match result {
                        Ok(result) => {
                            *state = QueryState::Complete {
                                rows: result.rows.len(),
                                bytes: result.bytes,
                                elapsed: result.elapsed,
                                rows_affected: result.rows_affected,
                            };
                            results.update(cx, |table, cx| {
                                let sort = sort_columns(&keys, &result.columns);
                                *table.delegate_mut() =
                                    ResultGrid::new(result).with_sort(sort, sortable);
                                table.refresh(cx);
                            });
                        }
                        Err(error) => *state = QueryState::Failed(error),
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn sql_to_run(
        &self,
        editor: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
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
        let form = self.form.as_ref().expect("form is rendered only while open");
        let message = form.error.clone();
        let hairline = || div().h(px(1.)).flex_1().bg(t.border);

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(layout::DIALOG_WIDTH))
                    .p(px(layout::SPACE_LG))
                    .bg(t.panel)
                    .border_1()
                    .border_color(t.border)
                    .rounded(px(layout::RADIUS_PANEL))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(layout::SPACE_MD))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(layout::SPACE_MD))
                            .child(
                                div()
                                    .size(px(28.))
                                    .flex_shrink_0()
                                    .rounded(px(layout::RADIUS_CONTROL))
                                    .bg(t.element_active)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        icon(icon::DATABASE)
                                            .size(px(layout::ICON_SIZE))
                                            .text_color(t.accent),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_size(px(layout::TEXT_LG))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Connect to Postgres"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(layout::TEXT_SM))
                                            .text_color(t.text_muted)
                                            .child("Paste a URL, or fill in the fields."),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(layout::SPACE_XS))
                            .child(
                                div()
                                    .text_size(px(layout::TEXT_SM))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(t.text_muted)
                                    .child("Connection URL"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap(px(layout::SPACE_SM))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .child(Input::new(&form.url).w_full()),
                                    )
                                    .child(
                                        Button::new("apply-connection-url")
                                            .icon(icon(icon::FILL_DOWN))
                                            .tooltip("Fill the fields from this URL")
                                            .on_click(cx.listener(Self::apply_connection_url)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(layout::SPACE_SM))
                            .child(hairline())
                            .child(
                                div()
                                    .text_size(px(layout::TEXT_XS))
                                    .text_color(t.text_faint)
                                    .child("OR"),
                            )
                            .child(hairline()),
                    )
                    .child(self.form_field("Display name", &form.name, cx))
                    .child(
                        div()
                            .flex()
                            .gap(px(layout::SPACE_SM))
                            .child(div().flex_1().child(self.form_field("Host", &form.host, cx)))
                            .child(div().w(px(96.)).child(self.form_field(
                                "Port",
                                &form.port,
                                cx,
                            ))),
                    )
                    .child(self.form_field("Database", &form.database, cx))
                    .child(self.form_field("Username", &form.user, cx))
                    .child(self.form_field("Password", &form.password, cx))
                    .children(message.map(|message| {
                        div()
                            .text_size(px(layout::TEXT_SM))
                            .text_color(t.danger)
                            .child(message)
                    }))
                    .child(
                        Button::new("connect")
                            .label("Connect")
                            .primary()
                            .w_full()
                            .on_click(cx.listener(Self::connect)),
                    ),
            )
    }

    fn form_field(
        &self,
        label: &'static str,
        input: &Entity<InputState>,
        cx: &App,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(layout::SPACE_XS))
            .child(
                div()
                    .text_size(px(layout::TEXT_SM))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme(cx).text_muted)
                    .child(label),
            )
            .child(Input::new(input).w_full())
    }

    fn render_main_content(profile: &Profile, cx: &mut Context<Self>) -> AnyElement {
        let t = *theme(cx);
        let body = match profile.session.active_object() {
            Some(tab) => Self::render_object(tab, cx),
            None => Self::render_query_surface(profile, cx),
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            // Chrome, so the strip reads as the frame the surfaces sit in.
            .bg(t.surface)
            .child(Self::render_tab_strip(profile, cx))
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
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = *theme(cx);
        let mono = gpui_component::Theme::global(cx).mono_font_family.clone();

        // The editor is the prompt, one tone behind its results.
        let top = div()
            .size_full()
            .bg(t.panel)
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
            (layout::EDITOR_DEFAULT_HEIGHT, layout::RESULTS_DEFAULT_HEIGHT)
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

    fn render_query_surface(profile: &Profile, cx: &mut Context<Self>) -> AnyElement {
        let bottom = Self::render_results(&profile.session.query, &profile.session.results, true, cx);
        Self::render_editor_surface(
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
    fn render_object(tab: &ObjectTab, cx: &mut Context<Self>) -> AnyElement {
        let t = *theme(cx);
        let ObjectBody::Relation {
            showing_structure,
            structure,
            results,
            query,
            ..
        } = &tab.body
        else {
            return Self::render_routine(tab, cx);
        };

        if *showing_structure {
            return div()
                .size_full()
                .min_h_0()
                .bg(t.bg)
                .child(Self::render_structure(structure, cx))
                .into_any_element();
        }

        Self::render_results(query, results, false, cx)
    }

    fn render_routine(tab: &ObjectTab, cx: &mut Context<Self>) -> AnyElement {
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
            .bg(t.panel)
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
                            .children((!routine.result_type.is_empty()).then(|| {
                                div().child(format!("Returns: {}", routine.result_type))
                            }))
                            .child(
                                div()
                                    .ml_auto()
                                    .child(key_hint(t, "escape", "returns to the editor")),
                            ),
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
        cx: &mut Context<Self>,
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

        let content = match query {
            QueryState::Idle if is_query => centered(
                key_hint(t, "cmd-enter", "runs the selection or statement under the cursor")
                    .into_any_element(),
            ),
            // A preview runs the moment its tab is shown, so an idle one is a
            // tab that is about to run rather than one waiting to be asked.
            QueryState::Idle | QueryState::Running => centered(quiet_line("Running query…".into())),
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
                        .child(Table::new(results).bordered(false).stripe(true)),
                )
                .children(Self::render_row_inspector(results, cx))
                .into_any_element(),
        };

        div()
            .size_full()
            .min_h_0()
            .bg(t.bg)
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
        cx: &mut Context<Self>,
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
                // One tone behind the results: this describes the data rather
                // than being it.
                .bg(t.panel)
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
                                Button::new("close-row-inspector")
                                    .icon(icon(icon::CLOSE))
                                    .ghost()
                                    .xsmall()
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
        cx: &mut Context<Self>,
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
            .child(icon(path).size(px(12.)).text_color(if selected {
                t.text
            } else {
                t.text_faint
            }))
            .child(label)
            .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
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

        let heading =
            |label: &'static str| div().pt(px(layout::SPACE_MD)).child(section_label(t, label));
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

    /// The tab strip. It sits directly above the editor and starts where the
    /// editor's text does, so a tab labels the surface under it rather than the
    /// window: the active one is lifted to the editor's tone, the rest are names
    /// that reveal a wash on hover. No boxes, no hairlines — tone carries the
    /// state.
    fn render_tab_strip(profile: &Profile, cx: &mut Context<Self>) -> AnyElement {
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

        tabs.extend(session.saved_queries.iter().enumerate().map(|(index, name)| {
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
                                .group_hover(format!("query-tab-{index}"), |style| style.opacity(1.))
                        })
                        .child(
                            Button::new(("delete-query", index))
                                .label(if pending { "Delete?" } else { "" })
                                .icon(icon(icon::DELETE))
                                .ghost()
                                .xsmall()
                                .tooltip("Delete query")
                                .on_click(move |_, _, cx| {
                                    // Or the chip underneath opens the query in
                                    // the same click, and the confirmation this
                                    // arms is cleared before it can be seen.
                                    cx.stop_propagation();
                                    _ = delete_workspace.update(cx, |workspace, cx| {
                                        workspace.delete_saved_query(delete_name.clone(), cx);
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
        }));

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
                            Button::new(("close-object", id as usize))
                                .icon(icon(icon::CLOSE))
                                .ghost()
                                .xsmall()
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
                    Button::new("confirm-save-query")
                        .icon(icon(if naming_a_rename {
                            icon::RENAME
                        } else {
                            icon::SAVE
                        }))
                        .small()
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
        let structure_toggle = session
            .active_object()
            .and_then(|tab| match &tab.body {
                ObjectBody::Relation {
                    showing_structure, ..
                } => Some(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .gap(px(layout::SPACE_XS))
                        .child(Self::preview_tab(
                            "Data",
                            icon::TABLE,
                            !showing_structure,
                            cx,
                        ))
                        .child(Self::preview_tab(
                            "Structure",
                            icon::STRUCTURE,
                            *showing_structure,
                            cx,
                        )),
                ),
                ObjectBody::Routine(_) => None,
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
                        Button::new("new-query-tab")
                            .icon(icon(icon::PLUS))
                            .ghost()
                            .xsmall()
                            .tooltip_with_action("New query", &NewQuery, None)
                            .on_click(move |_, window, cx| {
                                _ = new_workspace.update(cx, |workspace, cx| {
                                    workspace.new_query(&NewQuery, window, cx);
                                });
                            }),
                    ),
            )
            .children(structure_toggle)
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
                Button::new("rename-query")
                    .icon(icon(icon::RENAME))
                    .ghost()
                    .xsmall()
                    .tooltip("Rename query")
                    .on_click(move |_, window, cx| {
                        _ = rename_workspace.update(cx, |workspace, cx| {
                            workspace.rename_query(window, cx);
                        });
                    })
            }))
            .children((runnable && !session.naming && !named).then(|| {
                Button::new("save-query")
                    .icon(icon(icon::SAVE))
                    .ghost()
                    .xsmall()
                    .tooltip_with_action("Save query", &SaveQuery, None)
                    .on_click(move |_, window, cx| {
                        _ = save_workspace.update(cx, |workspace, cx| {
                            workspace.save_query(&SaveQuery, window, cx);
                        });
                    })
            }))
            .children(runnable.then(|| {
                Button::new("run-query")
                    .icon(icon(icon::RUN))
                    .ghost()
                    .xsmall()
                    .tooltip_with_action("Run", &RunQuery, None)
                    .on_click(move |_, window, cx| {
                        _ = run_workspace.update(cx, |workspace, cx| {
                            workspace.run_query(&RunQuery, window, cx);
                        });
                    })
            }))
            .into_any_element()
    }

    /// The connection switcher: a bottom-anchored row that opens a floating
    /// panel above itself, the way an account switcher floats over a sidebar,
    /// rather than an accordion that shoves the tree around.
    fn render_profile_switcher(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *theme(cx);
        let workspace = cx.entity().downgrade();
        let panel = self.switcher_open.then(|| {
            let add_workspace = workspace.clone();
            let profile_rows = self
                .profiles
                .iter()
                .enumerate()
                .map(|(index, profile)| {
                    let activate_workspace = workspace.clone();
                    let remove_workspace = workspace.clone();
                    let pending = self.pending_removal.as_deref() == Some(&profile.id);
                    let active = index == self.active;
                    div()
                        .id(("profile", index))
                        .group(format!("profile-row-{index}"))
                        .h(px(30.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_SM))
                        .px(px(layout::SPACE_SM))
                        .rounded(px(layout::RADIUS_CONTROL))
                        .hover(|style| style.bg(t.element_hover))
                        .child(row_icon(t, icon::DATABASE))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(profile.name.clone()),
                        )
                        .child(
                            div()
                                .when(!pending, |remove| {
                                    remove.opacity(0.).group_hover(
                                        format!("profile-row-{index}"),
                                        |style| style.opacity(1.),
                                    )
                                })
                                .child(
                                    Button::new(("remove-profile", index))
                                        .label(if pending { "Remove?" } else { "" })
                                        .icon(icon(icon::DELETE))
                                        .ghost()
                                        .xsmall()
                                        .tooltip("Remove connection")
                                        .on_click(move |_, window, cx| {
                                            _ = remove_workspace.update(cx, |workspace, cx| {
                                                workspace.remove_profile(index, window, cx);
                                            });
                                        }),
                                ),
                        )
                        // The mark sits at the trailing edge like a menu's
                        // checkmark, after the affordances, where the eye ends.
                        .children(active.then(|| {
                            icon(icon::CHECK)
                                .size(px(layout::ICON_SIZE))
                                .text_color(t.text_muted)
                        }))
                        .on_click(move |_, _, cx| {
                            _ = activate_workspace.update(cx, |workspace, cx| {
                                workspace.activate(index, cx);
                            });
                        })
                        .into_any_element()
                })
                .collect::<Vec<_>>();

            div()
                .absolute()
                .bottom(px(layout::SWITCHER_HEIGHT + layout::SPACE_XS))
                .left(px(layout::SPACE_SM))
                .right(px(layout::SPACE_SM))
                .p(px(layout::SPACE_XS))
                .bg(t.overlay)
                .border_1()
                .border_color(t.border_strong)
                .rounded(px(layout::RADIUS_PANEL))
                .shadow_lg()
                .flex()
                .flex_col()
                .child(
                    div()
                        .px(px(layout::SPACE_SM))
                        .py(px(layout::SPACE_XS))
                        .child(section_label(t, "Connections")),
                )
                .children(profile_rows)
                .child(
                    div()
                        .my(px(layout::SPACE_XS))
                        .h(px(1.))
                        .bg(t.border),
                )
                .child(
                    div()
                        .id("new-connection")
                        .h(px(30.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_SM))
                        .px(px(layout::SPACE_SM))
                        .rounded(px(layout::RADIUS_CONTROL))
                        .text_color(t.text_muted)
                        .hover(|style| style.bg(t.element_hover).text_color(t.text))
                        .child(row_icon(t, icon::PLUS))
                        .child("Add connection")
                        .on_click(move |_, window, cx| {
                            _ = add_workspace.update(cx, |workspace, cx| {
                                workspace.form = Some(ConnectionForm::new(None, window, cx));
                                workspace.switcher_open = false;
                                cx.notify();
                            });
                        }),
                )
        });
        let active_name = self
            .profile()
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| "Connections".into());
        let toggle_workspace = workspace.clone();

        div()
            .relative()
            .flex_shrink_0()
            .children(panel)
            .child(
                div()
                    .id("profile-switcher")
                    .h(px(layout::SWITCHER_HEIGHT))
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .px(px(layout::SPACE_MD))
                    .hover(|style| style.bg(t.element_hover))
                    .child(row_icon(t, icon::DATABASE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .font_weight(FontWeight::MEDIUM)
                            .child(active_name),
                    )
                    .child(row_icon(t, icon::SWITCHER))
                    .on_click(move |_, _, cx| {
                        _ = toggle_workspace.update(cx, |workspace, cx| {
                            workspace.switcher_open = !workspace.switcher_open;
                            workspace.pending_removal = None;
                            cx.notify();
                        });
                    }),
            )
            .into_any_element()
    }

    fn render_explorer(&self, profile: &Profile, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *theme(cx);
        let workspace = cx.entity().downgrade();
        let leaves = profile.session.explorer_leaves.clone();
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
                render_tree(
                    &profile.session.explorer_tree,
                    move |index, entry, _, _, cx| {
                        let t = *theme(cx);
                        let leaf = leaves.get(entry.item().id.as_str()).copied();
                        let label = entry.item().label.clone();
                        // Three ranks, three weights: a schema owns the column, a
                        // category only labels the run of objects under it, and the
                        // objects themselves are what the eye is actually hunting for.
                        let (label, row) = match (leaf, entry.depth()) {
                            (Some(_), _) => (label, ListItem::new(index).text_color(t.text)),
                            (None, 0) => (
                                label,
                                ListItem::new(index)
                                    .text_color(t.text)
                                    .font_weight(FontWeight::SEMIBOLD),
                            ),
                            (None, _) => (
                                label.to_uppercase().into(),
                                ListItem::new(index)
                                    .text_color(t.text_faint)
                                    .text_size(px(layout::TEXT_XS))
                                    .font_weight(FontWeight::MEDIUM),
                            ),
                        };
                        // A folder shows which way it is facing; an object shows what
                        // kind of object it is. Both occupy the same slot, so the
                        // labels line up down the column either way.
                        let row_icon_path = match leaf {
                            Some(leaf) => object_icon(leaf.kind),
                            None if entry.is_expanded() => icon::CHEVRON_DOWN,
                            None => icon::CHEVRON_RIGHT,
                        };
                        let row = row
                            .mx(px(layout::SPACE_XS))
                            .rounded(px(layout::RADIUS_CONTROL))
                            .pl(px(
                                layout::SPACE_SM + entry.depth() as f32 * layout::SPACE_MD
                            ))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(layout::SPACE_SM))
                                    .min_w_0()
                                    .flex_1()
                                    .child(row_icon(t, row_icon_path))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .whitespace_nowrap()
                                            .child(label),
                                    ),
                            );
                        let Some(leaf) = leaf else {
                            return row;
                        };
                        let workspace = workspace.clone();
                        // One click opens a tab to look at; the second keeps
                        // it. Both events arrive, so the double click promotes
                        // the tab its own first click opened.
                        row.on_click(move |event: &ClickEvent, window, cx| {
                            let transient = event.click_count() < 2;
                            _ = workspace.update(cx, |workspace, cx| {
                                workspace.open_explorer_target(leaf.target, transient, window, cx);
                            });
                        })
                    },
                )
                .into_any_element()
            }
        };

        div()
            .size_full()
            .h_full()
            .flex()
            .flex_col()
            // No border of its own: the resizable split's handle already
            // paints the one hairline this edge gets.
            .child(
                // A quiet filter row rather than a boxed field: on chrome, an
                // outlined input is the loudest thing in the column, and the
                // filter is the least interesting thing in it.
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
                    .child(
                        Input::new(&profile.session.explorer_filter)
                            .min_w_0()
                            .flex_1()
                            .appearance(false),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .py(px(layout::SPACE_XS))
                    .child(content),
            )
            .child(self.render_profile_switcher(cx))
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *theme(cx);
        // Deferred to render for the `&mut Window` a background task does not
        // have: the catalog that names these tabs resolves off-thread, and a
        // grid cannot be built without a window.
        self.restore_objects(window, cx);

        // Deferred for the same reason plus one: an element has to be mounted
        // before it can take focus.
        let take_focus = self.profile_mut().and_then(|profile| {
            if profile.session.naming {
                let wanted = profile.session.save_name_needs_focus;
                return wanted.then(|| {
                    profile.session.save_name_needs_focus = false;
                    Focus::Buffer(profile.session.save_name.clone())
                });
            }
            if !profile.session.editor_needs_focus {
                return None;
            }
            // Whatever the surface in front is: a keystroke reaches the
            // workspace along the focused element's dispatch path, so a
            // surface with nothing focused makes every keybinding dead.
            let focus = match profile.session.active {
                Tab::Query => Focus::Buffer(profile.session.editor.clone()),
                Tab::Object(id) => {
                    let tab = profile
                        .session
                        .objects
                        .iter()
                        .find(|tab| tab.id == id)?;
                    match &tab.body {
                        ObjectBody::Relation { results, .. } => Focus::Grid(results.clone()),
                        // A routine's tab is read. Nothing in it takes a
                        // keystroke, so nothing in it takes focus either.
                        ObjectBody::Routine(_) => return None,
                    }
                }
            };
            profile.session.editor_needs_focus = false;
            Some(focus)
        });
        match take_focus {
            Some(Focus::Buffer(input)) => input.focus_handle(cx).focus(window),
            Some(Focus::Grid(grid)) => grid.focus_handle(cx).focus(window),
            None => {}
        }

        if self.form.is_some() {
            return div()
                .id("connection-form")
                .size_full()
                // Chrome, so the form's card is the raised plane on it.
                .bg(t.surface)
                .text_color(t.text)
                .text_size(px(layout::TEXT_MD))
                .flex()
                .flex_col()
                .on_action(cx.listener(Self::cycle_theme))
                .on_action(cx.listener(Self::show_editor))
                .on_action(cx.listener(Self::next_profile))
                .on_action(cx.listener(Self::previous_profile))
                // Without a titlebar of its own the form has no drag handle at
                // all, since the platform's is transparent.
                .child(titlebar(t, None))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(self.render_connection_form(cx)),
                );
        }
        let Some(profile) = self.profile() else {
            unreachable!("the connection form is open when there are no profiles");
        };
        let failed = matches!(profile.state, ProfileState::Failed(_));
        let (status, status_color) = match &profile.state {
            ProfileState::Idle => ("Connection is idle.".to_string(), t.text_muted),
            ProfileState::Connecting => (
                format!("Connecting to {}…", profile.config.endpoint()),
                t.text_muted,
            ),
            ProfileState::Connected(_) => (
                format!("{} · {}", profile.name, profile.config.endpoint()),
                t.success,
            ),
            ProfileState::Failed(message) => (message.clone(), t.danger),
        };

        let query_status = match profile.session.active_query() {
            Some(QueryState::Complete {
                rows,
                bytes,
                elapsed,
                ..
            }) => Some(format!(
                "{} {} · {} · {elapsed:.1?}",
                group_thousands(*rows as u64),
                if *rows == 1 { "row" } else { "rows" },
                human_bytes(*bytes as u64),
            )),
            _ => None,
        };
        let notice = profile.session.notice.clone();

        div()
            .id("workspace")
            .on_action(cx.listener(Self::run_query))
            .on_action(cx.listener(Self::sort_column))
            .on_action(cx.listener(Self::show_editor))
            .on_action(cx.listener(Self::cycle_theme))
            .on_action(cx.listener(Self::save_query))
            .on_action(cx.listener(Self::new_query))
            .on_action(cx.listener(Self::next_profile))
            .on_action(cx.listener(Self::previous_profile))
            .on_action(cx.listener(Self::open_connection_form))
            .on_action(cx.listener(Self::zoom_editor_in))
            .on_action(cx.listener(Self::zoom_editor_out))
            .on_action(cx.listener(Self::reset_editor_zoom))
            .size_full()
            // The shell is the chrome tone: titlebar, sidebar and status bar
            // paint nothing of their own, they are this. The editor and the
            // results step forward from it by tone.
            .bg(t.surface)
            .text_color(t.text)
            .text_size(px(layout::TEXT_MD))
            .flex()
            .flex_col()
            .child(titlebar(t, Some(profile.name.clone())))
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("workspace-shell-split")
                        .child(
                            resizable_panel()
                                .size(px(layout::SIDEBAR_DEFAULT_WIDTH))
                                .size_range(
                                    px(layout::SIDEBAR_MIN_WIDTH)..px(layout::SIDEBAR_MAX_WIDTH),
                                )
                                .child(self.render_explorer(profile, cx)),
                        )
                        .child(
                            // Flush, not a floating card: the split handle
                            // already draws the one seam, and the planes
                            // inside separate by tone.
                            resizable_panel().child(
                                div()
                                    .size_full()
                                    .min_w_0()
                                    .bg(t.bg)
                                    .child(Self::render_main_content(profile, cx)),
                            ),
                        ),
                ),
            )
            .child(
                div()
                    .h(px(layout::STATUS_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .px(px(layout::SPACE_MD))
                    .text_size(px(layout::TEXT_SM))
                    // The dot carries the state and the text carries the words.
                    // A whole status line in green shouts about being connected,
                    // which is the least interesting thing Slate can tell you.
                    .child(
                        div()
                            .size(px(layout::SPACE_XS + 2.))
                            .rounded_full()
                            .bg(status_color),
                    )
                    .child(
                        div()
                            .text_color(if failed { t.danger } else { t.text_muted })
                            .child(status),
                    )
                    .children(notice.map(|notice| {
                        div().text_color(t.text_muted).child(notice)
                    }))
                    .children(query_status.map(|query_status| {
                        div().ml_auto().text_color(t.text_faint).child(query_status)
                    })),
            )
    }
}

/// The icon a sidebar row carries: a grid for a table, layers for one split
/// into partitions, an eye for the kinds that are a saved query over a table, a
/// disk for the one that stores its answer, and a globe for the one that lives
/// on another server entirely.
/// Slate's statement for a relation's tab, carrying the sort the headers asked
/// for. Regenerated rather than edited, so the row limit and the quoting stay
/// in one place.
fn relation_sql(schema: &str, relation: &str, sort: &[SortKey]) -> String {
    let preview = preview_sql(schema, relation);
    sql::with_order_by(&preview, sort).unwrap_or(preview)
}

/// How a column is named in an `ORDER BY`.
///
/// By name, so the statement reads as something a person would have written --
/// except where a name cannot identify one column, and then by position, which
/// always can. Duplicate names come back from any join written with `*`.
fn sort_expression(columns: &[db::Column], column: usize) -> Option<String> {
    let name = &columns.get(column)?.name;
    let unique = columns
        .iter()
        .filter(|other| &other.name == name)
        .count()
        == 1;

    Some(match unique && !name.is_empty() {
        true => format!("\"{}\"", name.replace('"', "\"\"")),
        false => (column + 1).to_string(),
    })
}

/// One header click against a sort: append, turn around, or drop out.
fn cycle(keys: &mut Vec<SortKey>, expression: &str) {
    match keys.iter().position(|key| key.expression == expression) {
        None => keys.push(SortKey::new(expression, true)),
        Some(index) if keys[index].ascending => keys[index].ascending = false,
        Some(index) => {
            keys.remove(index);
        }
    }
}

/// Which of a result's columns the statement ordered by, for the headers to
/// show. A key naming something other than a column of the result -- an
/// expression, or a column that is not in the select list -- lights nothing up,
/// because there is no header for it.
fn sort_columns(keys: &[SortKey], columns: &[db::Column]) -> Vec<(usize, bool)> {
    keys.iter()
        .filter_map(|key| {
            let expression = key.expression.trim();
            let named = unquote(expression);
            let column = columns
                .iter()
                .position(|column| column.name == named)
                .or_else(|| {
                    expression
                        .parse::<usize>()
                        .ok()
                        .filter(|position| (1..=columns.len()).contains(position))
                        .map(|position| position - 1)
                })?;
            Some((column, key.ascending))
        })
        .collect()
}

fn unquote(expression: &str) -> String {
    match expression.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) {
        Some(inner) => inner.replace("\"\"", "\""),
        None => expression.to_string(),
    }
}

fn object_icon(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Relation(RelationKind::Table) => icon::TABLE,
        ObjectKind::Relation(RelationKind::PartitionedTable) => icon::PARTITIONED_TABLE,
        ObjectKind::Relation(RelationKind::View) => icon::VIEW,
        ObjectKind::Relation(RelationKind::MaterializedView) => icon::MATERIALIZED_VIEW,
        ObjectKind::Relation(RelationKind::ForeignTable) => icon::FOREIGN_TABLE,
        ObjectKind::Routine(RoutineKind::Function) => icon::FUNCTION,
        ObjectKind::Routine(RoutineKind::Procedure) => icon::PROCEDURE,
    }
}

/// Write a profile's editor back to whichever file it came from.
fn write_buffer(profile: &Profile, cx: &App) -> Result<(), String> {
    let sql = profile.session.editor.read(cx).value().to_string();
    match &profile.session.open_query {
        Some(name) => store::write_query(&profile.id, name, &sql),
        None => store::write_scratch(&profile.id, &sql),
    }
}

/// One column of icons down the sidebar, so every label starts at the same x
/// whether its row is a folder or an object.
fn row_icon(t: Theme, path: &'static str) -> impl IntoElement {
    icon(path).size(px(layout::ICON_SIZE)).text_color(t.text_faint)
}

/// Slate's own titlebar, drawn where the platform's would be.
///
/// The system titlebar is transparent (see `main`), so this row is what runs to
/// the top of the window and the window buttons are drawn over its leading
/// inset. It is also the drag handle the platform no longer provides — which is
/// why nothing interactive lives here: a drag region swallows the clicks a
/// field or a button needs.
fn titlebar(t: Theme, subtitle: Option<String>) -> impl IntoElement {
    div()
        .id("titlebar")
        .window_control_area(gpui::WindowControlArea::Drag)
        .on_double_click(|_, window, _| window.titlebar_double_click())
        .h(px(layout::TITLEBAR_HEIGHT))
        .w_full()
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(layout::SPACE_MD))
        .pl(px(layout::TITLEBAR_LEADING_INSET))
        .pr(px(layout::SPACE_MD))
        .child(
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(layout::SPACE_SM))
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Slate"))
                .children(subtitle.map(|subtitle| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_XS))
                        .text_size(px(layout::TEXT_SM))
                        .text_color(t.text_faint)
                        .child(row_icon(t, icon::DATABASE))
                        .child(subtitle)
                })),
        )
}

/// The quietest thing on screen: small, uppercase, and dim enough that the
/// names under it are what the eye lands on first.
fn section_label(t: Theme, label: &str) -> impl IntoElement {
    div()
        .text_size(px(layout::TEXT_XS))
        .font_weight(FontWeight::MEDIUM)
        .text_color(t.text_faint)
        .child(label.to_uppercase())
}

/// A keycap, drawn the way the platform draws one in a menu. Reads a stroke in
/// GPUI's binding syntax so the hint and the binding cannot drift apart.
fn keycap(stroke: &'static str) -> Kbd {
    Kbd::new(Keystroke::parse(stroke).expect("keycap strokes are compile-time constants"))
}

/// A shortcut hint and what it does, in the app face rather than the editor's
/// monospace -- these are sentences about the UI, not query output.
fn key_hint(t: Theme, stroke: &'static str, explanation: &'static str) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        .text_size(px(layout::TEXT_SM))
        .text_color(t.text_faint)
        .child(keycap(stroke))
        .child(explanation)
}

/// `1234567` → `1,234,567`. Row counts are read at a glance, and groups are
/// what keeps six digits legible.
fn group_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// Bytes at the precision a person reads them, not the count the server sent.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn adjusted_editor_font_size(current: f32, delta: f32) -> f32 {
    (current + delta).clamp(EDITOR_FONT_SIZE_MIN, EDITOR_FONT_SIZE_MAX)
}

fn editor_zoom_percent(font_size: f32) -> u32 {
    (font_size / EDITOR_FONT_SIZE_DEFAULT * 100.0).round() as u32
}

fn result_pane_is_expanded(query: &QueryState) -> bool {
    !matches!(query, QueryState::Idle)
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
        let theme = Theme::default();
        theme.apply_to_components(cx);
        cx.set_global(theme);
        cx.bind_keys([
            KeyBinding::new("cmd-enter", RunQuery, None),
            KeyBinding::new("cmd-s", SaveQuery, None),
            KeyBinding::new("cmd-n", NewQuery, None),
            KeyBinding::new("cmd-shift-n", NewConnection, None),
            KeyBinding::new("ctrl-tab", NextProfile, None),
            KeyBinding::new("ctrl-shift-tab", PreviousProfile, None),
            KeyBinding::new("escape", ShowEditor, None),
            KeyBinding::new("cmd-shift-t", CycleTheme, None),
            KeyBinding::new("cmd-+", ZoomEditorIn, None),
            KeyBinding::new("cmd-=", ZoomEditorIn, None),
            KeyBinding::new("cmd--", ZoomEditorOut, None),
            KeyBinding::new("cmd-0", ResetEditorZoom, None),
        ]);

        // The platform titlebar is kept only for its window buttons: a system
        // bar in its own grey above Slate's chrome is the seam every native app
        // avoids. Slate paints that strip itself, and the buttons sit over it.
        let options = WindowOptions {
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

        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns(names: &[&str]) -> Vec<db::Column> {
        names
            .iter()
            .map(|name| db::Column {
                name: (*name).to_string(),
                data_type: None,
            })
            .collect()
    }

    #[test]
    fn clicking_a_column_appends_then_turns_around_then_drops_out() {
        let mut keys = Vec::new();

        cycle(&mut keys, r#""a""#);
        assert_eq!(keys, vec![SortKey::new(r#""a""#, true)]);
        // A second column joins the first rather than replacing it: that is
        // what makes a compound sort reachable by clicking.
        cycle(&mut keys, r#""b""#);
        assert_eq!(
            keys,
            vec![SortKey::new(r#""a""#, true), SortKey::new(r#""b""#, true)]
        );
        cycle(&mut keys, r#""a""#);
        assert_eq!(keys[0], SortKey::new(r#""a""#, false));
        cycle(&mut keys, r#""a""#);
        assert_eq!(keys, vec![SortKey::new(r#""b""#, true)]);
    }

    #[test]
    fn a_column_is_named_in_the_order_by_unless_a_name_cannot_identify_it() {
        let unique = columns(&["id", "name"]);
        assert_eq!(sort_expression(&unique, 1), Some(r#""name""#.into()));

        // `SELECT *` across a join returns the same name twice, and ordering by
        // it would be ambiguous -- so the position, which never is.
        let duplicated = columns(&["id", "id"]);
        assert_eq!(sort_expression(&duplicated, 1), Some("2".into()));
        assert_eq!(sort_expression(&unique, 7), None);

        // A quote in a column name would otherwise end the identifier early.
        let odd = columns(&["we\"ird"]);
        assert_eq!(sort_expression(&odd, 0), Some("\"we\"\"ird\"".into()));
    }

    #[test]
    fn the_headers_read_the_sort_back_off_the_statement() {
        let result = columns(&["id", "name"]);
        let keys = vec![
            SortKey::new(r#""name""#, false),
            SortKey::new("1", true),
            // Neither of these has a header to light up.
            SortKey::new("lower(name)", true),
            SortKey::new("9", true),
        ];

        assert_eq!(sort_columns(&keys, &result), vec![(1, false), (0, true)]);
    }

    #[test]
    fn a_relations_statement_carries_its_sort_before_the_limit() {
        let sorted = relation_sql("public", "accounts", &[SortKey::new(r#""id""#, false)]);

        assert_eq!(
            sorted,
            r#"SELECT * FROM "public"."accounts" ORDER BY "id" DESC LIMIT 1000"#
        );
        // And the sort Slate wrote is the sort its headers show.
        assert_eq!(
            sort_columns(
                &sql::order_by(&sorted).unwrap(),
                &columns(&["id", "email"])
            ),
            vec![(0, false)]
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
    fn default_editor_size_is_reported_as_one_hundred_percent() {
        assert_eq!(editor_zoom_percent(EDITOR_FONT_SIZE_DEFAULT), 100);
    }

    #[test]
    fn result_pane_expands_as_soon_as_a_query_starts() {
        assert!(!result_pane_is_expanded(&QueryState::Idle));
        assert!(result_pane_is_expanded(&QueryState::Running));
    }

    #[test]
    fn row_counts_are_grouped_for_reading() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(5000), "5,000");
        assert_eq!(group_thousands(1234567), "1,234,567");
    }

    #[test]
    fn byte_counts_read_at_human_precision() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(578_923), "578.9 KB");
        assert_eq!(human_bytes(1_500_000), "1.5 MB");
    }
}
