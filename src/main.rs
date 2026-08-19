mod db;
mod explorer;
mod result_grid;
mod sql;
mod store;

mod icons;
mod theme;

use std::{borrow::Cow, collections::HashMap, sync::Arc};

use gpui::{
    AnyElement, App, AppContext, Application, ClickEvent, Context, Entity, EntityInputHandler,
    Focusable, FontWeight, InteractiveElement, IntoElement, KeyBinding, ParentElement, Render,
    StatefulInteractiveElement, Styled, TitlebarOptions, Window, WindowOptions, actions, div,
    point, px,
};
use gpui_component::{
    InteractiveElementExt, Root, Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    list::ListItem,
    table::{Table, TableState},
    tree::{TreeState, tree as render_tree},
};

use db::{
    Catalog, Connection, ConnectionConfig, DbError, RelationKind, Routine, RoutineKind, Structure,
};
use explorer::{ExplorerLeaf, ExplorerTarget, ObjectKind, preview_sql, tree as build_explorer_tree};
use icons::{Icons, icon};
use result_grid::ResultGrid;
use sql::Buffer;
use theme::{Theme, layout, theme};

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
    ]
);

const RETURN_HINT: &str = "esc returns to the editor";

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
        store::StoredProfile {
            id: self.id.clone(),
            name: self.name.clone(),
            host: self.config.host.clone(),
            port: self.config.port,
            database: self.config.database.clone(),
            user: self.config.user.clone(),
            open_query: self.session.open_query.clone(),
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
    results: Entity<TableState<ResultGrid>>,
    query: QueryState,
    content: Content,
    explorer_filter: Entity<InputState>,
    explorer_tree: Entity<TreeState>,
    explorer_leaves: Arc<HashMap<String, ExplorerLeaf>>,
    /// `cmd+enter` reaches the workspace only through the focused element's
    /// dispatch path, so an unfocused editor makes the primary keystroke dead.
    editor_needs_focus: bool,
    open_query: Option<String>,
    saved_queries: Vec<String>,
    save_name: Entity<InputState>,
    naming: bool,
    pending_delete: Option<String>,
    notice: Option<String>,
}

impl Session {
    fn new(
        id: String,
        open_query: Option<String>,
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
        cx.subscribe(&save_name, |workspace, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                workspace.confirm_save(cx);
            }
        })
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
                    .placeholder("Write SQL…")
                    .default_value(sql)
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
            explorer_leaves: Arc::new(HashMap::new()),
            editor_needs_focus: true,
            open_query,
            saved_queries,
            save_name,
            naming: false,
            pending_delete: None,
            notice: None,
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
        let session = Session::new(stored.id.clone(), stored.open_query, window, cx);
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
        let session = Session::new(id.clone(), None, window, cx);
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
        let Some(connection) = profile.connection() else {
            return;
        };
        let id = profile.id.clone();
        let generation = profile.generation;
        let structure_task = cx.background_executor().spawn({
            let (schema, relation) = (schema.clone(), relation.clone());
            async move { connection.structure(&schema, &relation) }
        });

        cx.spawn(async move |workspace, cx| {
            let result = structure_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    let Some(profile) = workspace.issued_to(&id, generation) else {
                        return;
                    };
                    // A second click while this was in flight has already
                    // replaced the surface, and one relation's columns under
                    // another's name is worse than no columns at all.
                    if let Content::Preview(preview) = &mut profile.session.content
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

    /// Swap to the next registered theme. Every colour Slate paints is read
    /// from the global at render time, so repainting is the whole change — and
    /// side-by-side comparison is the only honest way to pick between palettes.
    fn cycle_theme(&mut self, _: &CycleTheme, _: &mut Window, cx: &mut Context<Self>) {
        let next = theme(cx).next();
        next.apply_to_components(cx);
        cx.set_global(next);
        cx.refresh_windows();
    }

    /// Return to the editor. Without this the routine and preview surfaces are
    /// one-way doors, since they replace the editor entirely.
    fn show_editor(&mut self, _: &ShowEditor, _: &mut Window, cx: &mut Context<Self>) {
        if self.form.is_some() && !self.profiles.is_empty() {
            self.form = None;
            cx.notify();
            return;
        }
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

    fn persist_buffer(&self, cx: &App) -> Result<(), String> {
        let Some(profile) = self.profile() else {
            return Ok(());
        };
        let sql = profile.session.editor.read(cx).value().to_string();
        match &profile.session.open_query {
            Some(name) => store::write_query(&profile.id, name, &sql),
            None => store::write_scratch(&profile.id, &sql),
        }
    }

    fn save_query(&mut self, _: &SaveQuery, _: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        if profile.session.open_query.is_none() {
            profile.session.naming = true;
            profile.session.notice = None;
            cx.notify();
            return;
        }
        match self.persist_buffer(cx) {
            Ok(()) => self.note("Saved query.".into(), cx),
            Err(message) => self.note(message, cx),
        }
    }

    fn confirm_save(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let name = profile.session.save_name.read(cx).value().trim().to_string();
        if let Err(message) = store::validate_query_name(&name) {
            self.note(message, cx);
            return;
        }
        let id = profile.id.clone();
        let sql = profile.session.editor.read(cx).value().to_string();
        if let Err(message) = store::write_query(&id, &name, &sql) {
            self.note(message, cx);
            return;
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.open_query = Some(name.clone());
            profile.session.saved_queries = store::saved_queries(&id);
            profile.session.naming = false;
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
        profile.session.content = Content::Query;
        profile.session.query = QueryState::Idle;
        profile.session.naming = false;
        profile.session.notice = None;
        profile
            .session
            .editor
            .update(cx, |editor, cx| editor.set_value("", window, cx));
        profile.session.editor_needs_focus = true;
        self.remember_profiles(cx);
        cx.notify();
    }

    fn open_saved_query(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        profile.session.content = Content::Query;
        profile.session.query = QueryState::Idle;
        profile.session.editor_needs_focus = true;
        profile.session.notice = None;
        self.remember_profiles(cx);
        cx.notify();
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

        let Some(connection) = profile.connection() else {
            profile.session.query = QueryState::Failed(DbError {
                message: "The connection is not open.".into(),
                position: None,
            });
            cx.notify();
            return;
        };
        let id = profile.id.clone();
        let generation = profile.generation;
        let results = profile.session.results.clone();
        profile.session.query = QueryState::Running;

        // Rows from the previous statement must not sit under the one now on
        // screen -- a reader cannot tell stale rows from fresh ones.
        results.update(cx, |table, cx| {
            *table.delegate_mut() = ResultGrid::empty();
            table.refresh(cx);
        });
        cx.notify();

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
        let form = self.form.as_ref().expect("form is rendered only while open");
        let message = form.error.clone();

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
                    .child(
                        div()
                            .text_size(px(layout::TEXT_XL))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Connect to Postgres"),
                    )
                    .child(
                        div()
                            .text_size(px(layout::TEXT_SM))
                            .text_color(t.text_muted)
                            .child("Paste a connection URL or enter the profile fields."),
                    )
                    .child(self.form_field("Connection URL", &form.url, cx))
                    .child(
                        div().flex().justify_end().child(
                            Button::new("apply-connection-url")
                                .label("Use URL")
                                .on_click(cx.listener(Self::apply_connection_url)),
                        ),
                    )
                    .child(self.form_field("Display name", &form.name, cx))
                    .child(self.form_field("Host", &form.host, cx))
                    .child(self.form_field("Port", &form.port, cx))
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
                        .child(
                            div()
                                .text_size(px(layout::TEXT_LG))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!(
                                    "{}.{}({})",
                                    details.schema,
                                    details.routine.name,
                                    details.routine.identity_arguments
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(layout::SPACE_LG))
                                .text_size(px(layout::TEXT_SM))
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
                        .bg(t.panel)
                        .overflow_y_scroll()
                        .p(px(layout::SPACE_LG))
                        .font_family(mono)
                        .child(details.routine.definition.clone()),
                );
        }

        // The generated preview is shown as its own read-only surface, so it is
        // always distinguishable from SQL the user wrote.
        let top = match &profile.session.content {
            // Sized by its contents, unlike the editor: a generated `SELECT` is
            // three lines of chrome, and giving it half the pane leaves the rows
            // it was run to show squeezed into the bottom half.
            Content::Preview(preview) => div()
                .flex_shrink_0()
                .p(px(layout::SPACE_LG))
                .font_family(mono)
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_SM))
                .child(
                    div()
                        .flex()
                        .gap(px(layout::SPACE_SM))
                        .child(section_label(t, "Generated preview"))
                        .child(
                            div()
                                .text_size(px(layout::TEXT_XS))
                                .text_color(t.text_faint)
                                .child(RETURN_HINT),
                        ),
                )
                .child(preview.sql.clone())
                .child(
                    div()
                        .flex()
                        .gap(px(layout::SPACE_XS))
                        .child(Self::preview_tab(
                            "Data",
                            icon::TABLE,
                            !preview.showing_structure,
                            cx,
                        ))
                        .child(Self::preview_tab(
                            "Structure",
                            icon::STRUCTURE,
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

        // The grid and the message are alternatives, not layers. A `div` lays its
        // children out in a row, so a full-size message beside a full-size table
        // was pushed off the pane entirely -- every query error was invisible.
        let bottom = match &profile.session.content {
            Content::Preview(preview) if preview.showing_structure => {
                Self::render_structure(&preview.structure, cx)
            }
            _ if !result_lines.is_empty() => div()
                .size_full()
                .p(px(layout::SPACE_LG))
                .font_family(gpui_component::Theme::global(cx).mono_font_family.clone())
                .flex()
                .flex_col()
                .text_color(if matches!(profile.session.query, QueryState::Failed(_)) {
                    t.danger
                } else {
                    t.text_muted
                })
                .children(
                    result_lines
                        .into_iter()
                        .map(|line| div().w_full().py(px(layout::SPACE_XS)).child(line)),
                )
                .into_any_element(),
            // Values are read by comparing them down a column, which only lines
            // up in a monospaced face -- and the header inherits it, so the
            // heading of a column sits in the same rhythm as its values.
            _ => div()
                .size_full()
                .font_family(gpui_component::Theme::global(cx).mono_font_family.clone())
                .child(Table::new(&profile.session.results).bordered(false))
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
                    .bg(t.panel)
                    .border_t_1()
                    .border_color(t.border)
                    .child(bottom),
            )
    }

    fn preview_tab(
        label: &'static str,
        path: &'static str,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> Button {
        let button = Button::new(label).icon(icon(path)).label(label).small();
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

    fn render_saved_queries(profile: &Profile, cx: &mut Context<Self>) -> AnyElement {
        let t = *theme(cx);
        let workspace = cx.entity().downgrade();
        let mut rows = profile
            .session
            .saved_queries
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let open_name = name.clone();
                let delete_name = name.clone();
                let open_workspace = workspace.clone();
                let delete_workspace = workspace.clone();
                let pending = profile.session.pending_delete.as_deref() == Some(name);
                div()
                    .id(("saved-query", index))
                    .h(px(30.))
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .px(px(layout::SPACE_SM))
                    .text_color(t.text_muted)
                    .hover(|style| style.bg(t.element_hover))
                    .child(row_icon(t, icon::SAVED_QUERY))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(name.clone()),
                    )
                    .child(
                        Button::new(("delete-query", index))
                            .label(if pending { "Delete?" } else { "" })
                            .icon(icon(icon::DELETE))
                            .ghost()
                            .xsmall()
                            .on_click(move |_, _, cx| {
                                _ = delete_workspace.update(cx, |workspace, cx| {
                                    workspace.delete_saved_query(delete_name.clone(), cx);
                                });
                            }),
                    )
                    .on_click(move |_, window, cx| {
                        _ = open_workspace.update(cx, |workspace, cx| {
                            workspace.open_saved_query(open_name.clone(), window, cx);
                        });
                    })
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        if profile.session.naming {
            let workspace = workspace.clone();
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_XS))
                    .p(px(layout::SPACE_SM))
                    .child(Input::new(&profile.session.save_name).flex_1())
                    .child(
                        Button::new("confirm-save-query")
                            .label("Save")
                            .small()
                            .on_click(move |_, _, cx| {
                                _ = workspace.update(cx, |workspace, cx| {
                                    workspace.confirm_save(cx);
                                });
                            }),
                    )
                    .into_any_element(),
            );
        }

        div()
            .flex_shrink_0()
            .border_t_1()
            .border_color(t.border)
            .child(
                div()
                    .px(px(layout::SPACE_SM))
                    .pt(px(layout::SPACE_SM))
                    .child(section_label(t, "Saved queries")),
            )
            .child(
                div()
                    .id("saved-query-scroll")
                    .max_h(px(148.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .into_any_element()
    }

    fn render_profile_switcher(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *theme(cx);
        let workspace = cx.entity().downgrade();
        let profile_rows = self
            .profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                let activate_workspace = workspace.clone();
                let remove_workspace = workspace.clone();
                let pending = self.pending_removal.as_deref() == Some(&profile.id);
                div()
                    .id(("profile", index))
                    .h(px(34.))
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .px(px(layout::SPACE_SM))
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
                        Button::new(("remove-profile", index))
                            .label(if pending { "Remove?" } else { "" })
                            .icon(icon(icon::DELETE))
                            .ghost()
                            .xsmall()
                            .on_click(move |_, window, cx| {
                                _ = remove_workspace.update(cx, |workspace, cx| {
                                    workspace.remove_profile(index, window, cx);
                                });
                            }),
                    )
                    .on_click(move |_, _, cx| {
                        _ = activate_workspace.update(cx, |workspace, cx| {
                            workspace.activate(index, cx);
                        });
                    })
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let active_name = self
            .profile()
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| "Connections".into());
        let toggle_workspace = workspace.clone();
        let add_workspace = workspace.clone();

        div()
            .flex_shrink_0()
            .border_t_1()
            .border_color(t.border)
            .children(self.switcher_open.then(|| {
                div()
                    .border_b_1()
                    .border_color(t.border)
                    .children(profile_rows)
                    .child(
                        Button::new("new-connection")
                            .label("Add connection")
                            .icon(icon(icon::DATABASE))
                            .ghost()
                            .w_full()
                            .on_click(move |_, window, cx| {
                                _ = add_workspace.update(cx, |workspace, cx| {
                                    workspace.form =
                                        Some(ConnectionForm::new(None, window, cx));
                                    workspace.switcher_open = false;
                                    cx.notify();
                                });
                            }),
                    )
            }))
            .child(
                div()
                    .id("profile-switcher")
                    .h(px(40.))
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .px(px(layout::SPACE_SM))
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
                render_tree(&profile.session.explorer_tree, move |index, entry, _, _, cx| {
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
                row.on_click(move |_, _, cx| {
                    _ = workspace.update(cx, |workspace, cx| {
                        workspace.open_explorer_target(leaf.target, cx);
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
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(t.border)
            .child(
                div().p(px(layout::SPACE_SM)).child(
                    Input::new(&profile.session.explorer_filter)
                        .w_full()
                        .prefix(row_icon(t, icon::SEARCH)),
                ),
            )
            .child(div().flex_1().min_h_0().child(content))
            .child(Self::render_saved_queries(profile, cx))
            .child(self.render_profile_switcher(cx))
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

        if self.form.is_some() {
            return div()
                .id("connection-form")
                .size_full()
                .bg(t.bg)
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
        let notice = profile.session.notice.clone();

        div()
            .id("workspace")
            .on_action(cx.listener(Self::run_query))
            .on_action(cx.listener(Self::show_editor))
            .on_action(cx.listener(Self::cycle_theme))
            .on_action(cx.listener(Self::save_query))
            .on_action(cx.listener(Self::new_query))
            .on_action(cx.listener(Self::next_profile))
            .on_action(cx.listener(Self::previous_profile))
            .on_action(cx.listener(Self::open_connection_form))
            .size_full()
            // The shell is the chrome tone: titlebar, sidebar and status bar
            // paint nothing of their own, they are this. The content card below
            // is the plane that steps away from it.
            .bg(t.surface)
            .text_color(t.text)
            .text_size(px(layout::TEXT_MD))
            .flex()
            .flex_col()
            .child(titlebar(t, Some(profile.name.clone())))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_explorer(profile, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .p(px(layout::SPACE_SM))
                            .child(
                                div()
                                    .size_full()
                                    .overflow_hidden()
                                    .bg(t.bg)
                                    .border_1()
                                    .border_color(t.border)
                                    .rounded(px(layout::RADIUS_PANEL))
                                    .child(Self::render_main_content(profile, result_lines, cx)),
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
                    .border_t_1()
                    .border_color(t.border)
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

/// One column of icons down the sidebar, so every label starts at the same x
/// whether its row is a folder or an object.
fn row_icon(t: Theme, path: &'static str) -> impl IntoElement {
    icon(path).size(px(layout::ICON_SIZE)).text_color(t.text_faint)
}

/// Slate's own titlebar, drawn where the platform's would be.
///
/// The system titlebar is transparent (see `main`), so this row is what runs to
/// the top of the window and the window buttons are drawn over its leading
/// inset. It is also the drag handle the platform no longer provides.
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
        .pl(px(layout::TITLEBAR_LEADING_INSET))
        .pr(px(layout::SPACE_MD))
        .border_b_1()
        .border_color(t.border)
        .child(
            div()
                .flex()
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
        .child(
            div()
                .ml_auto()
                .text_size(px(layout::TEXT_XS))
                .text_color(t.text_faint)
                .child(format!("{} · ⌘⇧T", t.name)),
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
