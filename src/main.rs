mod completion;
mod db;
mod explorer;
mod export;
mod palette;
mod result_grid;
mod sql;
mod store;
mod views;

mod icons;
mod theme;
mod tls;

use std::{borrow::Cow, collections::HashMap, path::PathBuf, rc::Rc, sync::Arc};

use gpui::{
    Action, AnyElement, App, AppContext, Application, ClickEvent, ClipboardItem, Context, Entity,
    EntityInputHandler, FocusHandle, Focusable, FontWeight, InteractiveElement, IntoElement,
    KeyBinding, Keystroke, Menu, MenuItem, ParentElement, Render, StatefulInteractiveElement,
    Styled, TitlebarOptions, Window, WindowOptions, actions, div, point, prelude::FluentBuilder,
    px,
};
use gpui_component::{
    Disableable, IndexPath, InteractiveElementExt, Root,
    button::{Button, ButtonVariants},
    input::{CompletionProvider, Enter, IndentInline, Input, InputEvent, InputState, Position},
    kbd::Kbd,
    list::{List, ListEvent, ListItem, ListState},
    resizable::{h_resizable, resizable_panel},
    table::{TableEvent, TableState},
    tree::{TreeState, tree as render_tree},
};
use serde::Deserialize;

use completion::SchemaCompletions;
use db::{
    Catalog, Connection, ConnectionConfig, DbError, Engine, RelationKind, Routine, RoutineKind,
    ServerConfig, SslMode, Structure,
};
use explorer::{
    ExplorerLeaf, ExplorerTarget, ObjectKind, PREVIEW_ROW_LIMIT, preview_sql,
    tree as build_explorer_tree,
};
use export::Format;
use icons::{Icons, icon};
use palette::{Command, Mode as PaletteMode, Palette};
use result_grid::{PendingRow, ResultGrid};
use sql::{Buffer, SortKey};
use theme::{FontSlot, Fonts, Theme, fonts, layout, theme};

/// A header click. The column is the one in the grid; which statement it
/// belongs to is whatever surface is in front, because that is the grid the
/// click came from.
#[derive(Clone, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = slate, no_json)]
struct SortColumn {
    column: usize,
}

/// How many rows a relation's preview asks for. Slate's own statement carries
/// the limit, so the only thing to say is the number.
#[derive(Clone, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = slate, no_json)]
struct SetRowLimit {
    rows: usize,
}

actions!(
    slate,
    [
        RunQuery,
        CancelQuery,
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
        EditCell,
        CopyCell,
        ApplyEdits,
        DiscardEdits,
        FuzzyOpen,
        CommandPalette,
        PaletteNext,
        PalettePrevious,
        CloseTab,
        ToggleSidebar,
        AcceptCompletion,
        Quit,
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
        let active = self.session.active;
        let mut open_objects = self
            .session
            .objects
            .iter()
            .map(|tab| store::StoredObject {
                active: active == Tab::Object(tab.id),
                ..tab.stored()
            })
            .collect::<Vec<_>>();
        // Both lists, because a restore now opens the relations first and
        // leaves the routines pending: writing either one alone would drop the
        // other half of the session.
        open_objects.extend(self.session.pending_objects.iter().cloned());

        // A file engine writes no server fields and a server engine writes no
        // path, rather than either writing a blank the loader would have to
        // decide the meaning of.
        let server = self.config.server();
        store::StoredProfile {
            id: self.id.clone(),
            name: self.name.clone(),
            host: server.map(|server| server.host.clone()).unwrap_or_default(),
            port: server.and_then(|server| server.port),
            database: server
                .map(|server| server.database.clone())
                .unwrap_or_default(),
            user: server.map(|server| server.user.clone()).unwrap_or_default(),
            sslmode: server.map(|server| server.sslmode.as_str().to_string()),
            root_certificate: server.and_then(|server| server.root_certificate.clone()),
            engine: Some(self.config.engine().as_str().to_string()),
            path: match &self.config {
                ConnectionConfig::Sqlite { path, .. } => Some(path.clone()),
                _ => None,
            },
            editor_font_size: Some(self.session.editor_font_size),
            statement_timeout: Some(self.config.statement_timeout()),
            next_query_id: Some(self.session.next_query_id),
            // Nothing writes the legacy scalar any more; a buffer's name is a
            // property of its tab now. Kept on the stored shape only so a
            // profile written by an older build still loads with its buffer.
            open_query: None,
            open_queries: self
                .session
                .queries
                .iter()
                .map(|tab| tab.stored(self.session.active == Tab::Query(tab.id)))
                .collect(),
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
    /// The open query buffers, in strip order. Never empty: a profile always
    /// has somewhere to write, so the last unsaved buffer has no closed state
    /// to go to.
    queries: Vec<QueryTab>,
    objects: Vec<ObjectTab>,
    active: Tab,
    next_query_id: u64,
    next_object_id: u64,
    /// Object tabs read back from disk, held until the catalog can name them.
    pending_objects: Vec<store::StoredObject>,
    /// What completion has learned about this connection's columns.
    ///
    /// Not in the catalog, deliberately: fetching every column of every
    /// relation at connect is unbounded work for a database nobody has asked a
    /// question about yet. A relation lands here the first time a statement
    /// names it, so what is held is bounded by what was written.
    completion_columns: completion::ColumnCache,
    explorer_filter: Entity<InputState>,
    explorer_tree: Entity<TreeState>,
    explorer_leaves: Arc<HashMap<String, ExplorerLeaf>>,
    /// `cmd+enter` reaches the workspace only through the focused element's
    /// dispatch path, so an unfocused editor makes the primary keystroke dead.
    editor_needs_focus: bool,
    /// The same hazard for the name field: an unfocused input asks for a name
    /// nobody can type into.
    save_name_needs_focus: bool,
    saved_queries: Vec<String>,
    /// The statements this profile has run, newest first. Held rather than read
    /// off disk when the palette opens, for the reason `saved_queries` is: the
    /// list is wanted while a list is being built, which is a frame.
    history: Vec<String>,
    save_name: Entity<InputState>,
    naming: bool,
    pending_delete: Option<String>,
    /// The saved query `cmd+w` is asking about.
    ///
    /// A saved query has no closed state — it is in the strip while its file
    /// exists and gone when it does not — so closing its tab is deleting it,
    /// and it is the one tab that says so before it goes. Separate from
    /// `pending_delete`, which is the chip's own quieter two-click arming.
    pending_close: Option<String>,
    /// The close `cmd+w` is asking about, because the tab it names holds cell
    /// edits nobody has applied. Held as the target rather than the tab, so
    /// confirming runs exactly the close the keystroke had decided on --
    /// including a saved query's own second question.
    pending_discard: Option<CloseTarget>,
    notice: Option<String>,
    editor_font_size: f32,
    /// The generated batch a relation tab is showing before it runs. That tab
    /// has no buffer to put SQL in, so the modal is where the statement is on
    /// screen — and nothing runs until Run.
    apply_review: Option<ApplyReview>,
}

/// A generated `UPDATE` batch waiting to be read and run.
struct ApplyReview {
    /// The tab the edits came from, so the modal is shown over that surface
    /// and a run cannot land in another tab's grid.
    tab: Tab,
    sql: String,
}

impl Session {
    fn new(
        id: String,
        stored_queries: Vec<store::StoredQueryTab>,
        stored_next_query_id: u64,
        editor_font_size: f32,
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
        // A tab naming a query whose file has gone comes back as the unsaved
        // buffer it now is, rather than as a tab pointing at nothing.
        let mut stored_queries = stored_queries
            .into_iter()
            .map(|mut stored| {
                stored.name = stored.name.filter(|name| saved_queries.contains(name));
                stored
            })
            .collect::<Vec<_>>();
        if stored_queries.is_empty() {
            stored_queries.push(store::StoredQueryTab {
                id: 0,
                name: None,
                active: true,
            });
        }

        let active = stored_queries
            .iter()
            .find(|stored| stored.active)
            .unwrap_or(&stored_queries[0])
            .id;
        let next_query_id = next_query_id(stored_next_query_id, &stored_queries);

        let mut notice = None;
        let queries = stored_queries
            .iter()
            .map(|stored| {
                let (tab, failure) = QueryTab::restore(&id, stored, window, cx);
                notice = notice.take().or(failure);
                tab
            })
            .collect();

        Self {
            queries,
            objects: Vec::new(),
            active: Tab::Query(active),
            next_query_id,
            next_object_id: 0,
            pending_objects,
            completion_columns: completion::ColumnCache::default(),
            explorer_filter,
            explorer_tree: cx.new(|cx| TreeState::new(cx)),
            explorer_leaves: Arc::new(HashMap::new()),
            editor_needs_focus: true,
            save_name_needs_focus: false,
            saved_queries,
            history: store::history(&id),
            save_name,
            naming: false,
            pending_delete: None,
            pending_close: None,
            pending_discard: None,
            notice,
            editor_font_size,
            apply_review: None,
        }
    }

    fn query_tab(&self, id: u64) -> Option<&QueryTab> {
        self.queries.iter().find(|tab| tab.id == id)
    }

    fn query_tab_mut(&mut self, id: u64) -> Option<&mut QueryTab> {
        self.queries.iter_mut().find(|tab| tab.id == id)
    }

    /// The query buffer in front, or `None` when an object tab is.
    fn active_query_tab(&self) -> Option<&QueryTab> {
        match self.active {
            Tab::Query(id) => self.query_tab(id),
            Tab::Object(_) => None,
        }
    }

    /// The tab a buffer holding `name` is in, if one is open.
    fn tab_holding(&self, name: &str) -> Option<u64> {
        self.queries
            .iter()
            .find(|tab| tab.open_query.as_deref() == Some(name))
            .map(|tab| tab.id)
    }

    /// The name of the buffer in front, when it has one.
    fn open_query(&self) -> Option<&str> {
        self.active_query_tab()
            .and_then(|tab| tab.open_query.as_deref())
    }

    fn active_object(&self) -> Option<&ObjectTab> {
        match self.active {
            Tab::Object(id) => self.objects.iter().find(|tab| tab.id == id),
            Tab::Query(_) => None,
        }
    }

    /// The query state behind the visible surface, or `None` for a surface that
    /// runs nothing — a routine is read, never executed by being opened.
    fn active_query(&self) -> Option<&QueryState> {
        match self.active_object() {
            None => self.active_query_tab().map(|tab| &tab.query),
            Some(tab) => match &tab.body {
                ObjectBody::Relation { query, .. } => Some(query),
                ObjectBody::Routine(_) => None,
            },
        }
    }

    /// The grid the visible surface is showing. A routine's tab has none: it is
    /// read, not run.
    fn active_results(&self) -> Option<&Entity<TableState<ResultGrid>>> {
        match self.active_object() {
            None => self.active_query_tab().map(|tab| &tab.results),
            Some(tab) => match &tab.body {
                ObjectBody::Relation { results, .. } => Some(results),
                ObjectBody::Routine(_) => None,
            },
        }
    }

    /// The buffer a run reads from, which only the query tab has. An object tab
    /// shows an object: there is no SQL in front of the user to run.
    fn editor(&self, tab: Tab) -> Option<Entity<InputState>> {
        match tab {
            Tab::Query(id) => self.query_tab(id).map(|tab| tab.editor.clone()),
            Tab::Object(_) => None,
        }
    }

    /// The grid one named tab is showing. `slot` answers the same question but
    /// needs a `&mut`, and asking what a tab holds changes nothing.
    fn results(&self, tab: Tab) -> Option<&Entity<TableState<ResultGrid>>> {
        match tab {
            Tab::Query(id) => self.query_tab(id).map(|tab| &tab.results),
            Tab::Object(id) => match &self.objects.iter().find(|tab| tab.id == id)?.body {
                ObjectBody::Relation { results, .. } => Some(results),
                ObjectBody::Routine(_) => None,
            },
        }
    }

    /// Where a run's state and rows belong. Returning both together is what
    /// keeps a result from landing in one tab's grid with another tab's status.
    fn slot(&mut self, tab: Tab) -> Option<(&mut QueryState, Entity<TableState<ResultGrid>>)> {
        match tab {
            Tab::Query(id) => {
                let tab = self.query_tab_mut(id)?;
                let results = tab.results.clone();
                Some((&mut tab.query, results))
            }
            Tab::Object(id) => match &mut self.objects.iter_mut().find(|tab| tab.id == id)?.body {
                ObjectBody::Relation { query, results, .. } => Some((query, results.clone())),
                ObjectBody::Routine(_) => None,
            },
        }
    }

    /// Drop every confirmation and half-finished prompt this session is holding.
    ///
    /// All four name the buffer or tab they were raised over, so leaving one
    /// standing across a context change offers to delete one query while another
    /// is on screen. One method rather than a clear at each site, because the
    /// two callers had drifted apart: switching profiles left `naming` set with
    /// `save_name_needs_focus` already spent, which renders the name prompt and
    /// then hands focus to nothing at all — and a window with nothing focused
    /// has no dispatch path, so the keyboard goes dead until something is
    /// clicked.
    fn clear_prompts(&mut self) {
        self.pending_delete = None;
        self.pending_close = None;
        self.pending_discard = None;
        self.naming = false;
        self.save_name_needs_focus = false;
    }
}

/// What takes focus when a surface comes to the front. A buffer and a grid are
/// both focusable and neither is the other's type.
enum Focus {
    Buffer(Entity<InputState>),
    Grid(Entity<TableState<ResultGrid>>),
    /// The window itself, for a surface with nothing in it to type into. Not a
    /// no-op: a keystroke only reaches the workspace along the focused
    /// element's dispatch path, so focusing nothing at all is what makes every
    /// binding dead until something is clicked.
    Window,
}

enum CatalogState {
    Loading,
    Loaded(Catalog),
    Failed(String),
}

/// Which surface the main pane is showing, and what a run targets. Both kinds
/// of tab are addressed by id rather than by index, so closing one cannot land
/// an in-flight result in its neighbour's grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Query(u64),
    Object(u64),
}

/// What `cmd+w` has to do with the surface in front of it.
#[derive(Debug, PartialEq, Eq)]
enum CloseTarget {
    /// Close it. It is a view onto something the database still holds, and
    /// reopening it costs a click.
    Object(u64),
    /// Ask first. A saved query is listed while its file exists and gone when
    /// it does not, so closing its tab is deleting it.
    SavedQuery(String),
    /// Close it. An unsaved buffer that is not the last one is a scratch pad
    /// someone is done with; its text goes with it, which is what closing an
    /// unnamed buffer means everywhere else.
    Buffer(u64),
}

impl CloseTarget {
    /// The tab this close takes out of the strip. A saved query names its file
    /// rather than its tab, so the strip is what says which tab that is -- and
    /// `None` is a saved query with no tab open, which no close comes from.
    fn tab(&self, session: &Session) -> Option<Tab> {
        match self {
            Self::Object(id) => Some(Tab::Object(*id)),
            Self::Buffer(id) => Some(Tab::Query(*id)),
            Self::SavedQuery(name) => session.tab_holding(name).map(Tab::Query),
        }
    }
}

/// `None` for the last unsaved buffer, which is always in the strip: a profile
/// always has somewhere to write, so there is no closed state for it to go to
/// and `cmd+w` on it does nothing rather than inventing one.
fn close_target(active: Tab, open_query: Option<&str>, unsaved: usize) -> Option<CloseTarget> {
    match (active, open_query) {
        (Tab::Object(id), _) => Some(CloseTarget::Object(id)),
        (Tab::Query(_), Some(name)) => Some(CloseTarget::SavedQuery(name.to_string())),
        (Tab::Query(id), None) => (unsaved > 1).then_some(CloseTarget::Buffer(id)),
    }
}

/// One query buffer, and everything that belongs to it.
///
/// There used to be exactly one of these per profile, held directly on
/// `Session`, and opening a saved query swapped its text in place. That is why
/// `cmd+t` on a dirty scratch buffer persisted it and then cleared it: there
/// was nowhere else for it to be. A buffer is a tab now, the same way an object
/// is, and for the same reason -- addressed by id, so closing one cannot land
/// another's result in its grid.
struct QueryTab {
    id: u64,
    editor: Entity<InputState>,
    results: Entity<TableState<ResultGrid>>,
    query: QueryState,
    /// The saved query this buffer holds, or `None` while it is unsaved.
    ///
    /// It is also which file the buffer persists to: a name means the query
    /// file, no name means this tab's own scratch file.
    open_query: Option<String>,
    /// The statement behind this tab's grid.
    ///
    /// Held rather than derived from the buffer, unlike the sort path, and a
    /// deliberate exception to that rule (in-grid editing spec, §4): applying
    /// edits appends the `UPDATE` to the buffer, so the cursor no longer sits on
    /// the `SELECT` and the text can no longer say where these rows came from.
    last_query: Option<String>,
    /// Whether this tab's snapshot has been looked for yet. Set on the first
    /// attempt whether or not one was found, so a tab reached a second time
    /// cannot read the disk again and put stale rows over live ones.
    hydrated: bool,
}

impl QueryTab {
    /// Read one stored buffer back off disk.
    ///
    /// Returns the message rather than reporting it: several tabs are restored
    /// at once and the session shows one notice, so the caller decides which
    /// failure is the one worth saying. A buffer that could not be read is
    /// empty either way, and silence would make it look like it never held
    /// anything.
    fn restore(
        profile_id: &str,
        stored: &store::StoredQueryTab,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> (Self, Option<String>) {
        let text = match &stored.name {
            Some(name) => store::read_query(profile_id, name),
            None => store::read_scratch(profile_id, stored.id),
        };
        let (sql, notice) = match text {
            Ok(sql) => (sql.unwrap_or_default(), None),
            Err(message) => (String::new(), Some(message)),
        };

        let tab = Self {
            id: stored.id,
            editor: cx.new(|cx| {
                InputState::new(window, cx)
                    .code_editor("sql")
                    .soft_wrap(false)
                    .placeholder("Write SQL…")
                    .default_value(sql)
            }),
            results: result_grid(window, cx),
            query: QueryState::Idle,
            open_query: stored.name.clone(),
            last_query: None,
            hydrated: false,
        };
        (tab, notice)
    }

    fn stored(&self, active: bool) -> store::StoredQueryTab {
        store::StoredQueryTab {
            id: self.id,
            name: self.open_query.clone(),
            active,
        }
    }
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
    body: ObjectBody,
}

impl ObjectTab {
    fn stored(&self) -> store::StoredObject {
        store::StoredObject {
            schema: self.schema.clone(),
            name: self.name.clone(),
            routine: matches!(self.kind, ObjectKind::Routine(_)),
            kind: match self.kind {
                ObjectKind::Relation(kind) => kind,
                ObjectKind::Routine(_) => RelationKind::default(),
            },
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

/// What the catalog says a relation is. The only authority on it: a stored
/// tab's kind is a cache of this, and can be a default rather than a kind.
fn relation_kind(catalog: &Catalog, schema: &str, name: &str) -> Option<RelationKind> {
    catalog
        .schemas
        .iter()
        .find(|candidate| candidate.name == schema)?
        .relations
        .iter()
        .find(|relation| relation.name == name)
        .map(|relation| relation.kind)
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
        /// How many rows this preview asks for. Every result set is capped
        /// (spec §4.3); this is the tab's own copy of the cap, so raising it
        /// for one wide table does not raise it everywhere.
        limit: usize,
        /// Whether the rows on screen came off disk rather than from the
        /// server. Refreshed on the tab's first activation and cleared there,
        /// not at startup: a session of restored tabs would otherwise open by
        /// firing one query per tab at a database nobody has looked at yet.
        stale: bool,
        /// Whether this tab's snapshot has been looked for yet. See
        /// [`QueryTab::hydrated`].
        hydrated: bool,
    },
    Routine(Routine),
}

enum StructureState {
    Loading,
    Loaded(Structure),
    Failed(String),
}

/// Put a snapshot's rows on screen.
fn show_snapshot(
    results: &Entity<TableState<ResultGrid>>,
    grid: &store::StoredGrid,
    cx: &mut Context<Workspace>,
) {
    results.update(cx, |table, cx| {
        *table.delegate_mut() = ResultGrid::restored(grid);
        table.refresh(cx);
    });
}

/// The state a tab restored from a snapshot is in.
///
/// `Complete` rather than `Idle`, for two reasons: the rows are a result and
/// the status bar has to be able to count them, and it is what makes
/// `load_relation` leave a restored tab's rows alone instead of re-querying
/// them the moment the tab is reached. The cost fields are zero because a
/// snapshot knows none of them -- it is not the run, it is what the run left --
/// and the status readout says "snapshot" rather than reporting the zeroes.
///
/// `rows` is the result's own size, which can be larger than the rows the grid
/// holds: a snapshot is capped. `row_readout` is what says so, and
/// `export_results` refuses rather than writing a short file.
fn restored_state(grid: &store::StoredGrid) -> QueryState {
    QueryState::Complete {
        rows: grid.total_rows,
        bytes: 0,
        elapsed: std::time::Duration::ZERO,
        rows_affected: None,
    }
}

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

struct ConnectionForm {
    url: Entity<InputState>,
    /// Which set of fields below is the connection. Every input is built once
    /// and kept; the engine decides which are drawn and which are read, so
    /// switching engine and switching back does not lose what was typed.
    engine: Engine,
    name: Entity<InputState>,
    /// SQLite's entire connection. No host, no credentials, no transport.
    path: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    sslmode: SslMode,
    /// Only reachable while the mode consults one, so the field cannot sit
    /// there filled in and doing nothing.
    root_certificate: Entity<InputState>,
    /// Seconds, and blank is the same as 0: no limit. Every engine has one, so
    /// unlike the credential fields it is drawn whichever chip is selected.
    statement_timeout: Entity<InputState>,
    /// An input to focus once it has been mounted.
    ///
    /// A chip can unmount the field the user was typing in, and a window with
    /// nothing focused has no dispatch path — every keybinding in the app goes
    /// dead until something is clicked. So whichever chip takes a field away
    /// names the one that replaces it, and `Workspace::render` hands focus over
    /// on the next frame, once it exists to receive it.
    needs_focus: Option<Entity<InputState>>,
    error: Option<String>,
}

impl ConnectionForm {
    fn new(
        config: Option<&ConnectionConfig>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let value = |value: Option<&str>| value.unwrap_or_default().to_string();
        let server = config.and_then(ConnectionConfig::server);
        let file = match config {
            Some(ConnectionConfig::Sqlite { path, .. }) => Some(path.as_str()),
            _ => None,
        };

        let url =
            cx.new(|cx| InputState::new(window, cx).placeholder("postgresql://…  or  sqlite://…"));
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Display name")
                .default_value(value(
                    server
                        .map(|server| server.database.as_str())
                        .or_else(|| file.map(file_stem)),
                ))
        });
        let path = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Database file")
                .default_value(value(file))
        });
        let host = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Host")
                .default_value(value(server.map(|server| server.host.as_str())))
        });
        let port = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Port (optional)")
                .default_value(
                    server
                        .and_then(|server| server.port)
                        .map(|port| port.to_string())
                        .unwrap_or_default(),
                )
        });
        let database = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Database")
                .default_value(value(server.map(|server| server.database.as_str())))
        });
        let user = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Username")
                .default_value(value(server.map(|server| server.user.as_str())))
        });
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Password (optional)")
                .default_value(value(server.map(|server| server.password.as_str())))
                .masked(true)
        });

        let root_certificate = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Root certificate file (optional)")
                .default_value(value(
                    server.and_then(|server| server.root_certificate.as_deref()),
                ))
        });
        let statement_timeout = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Seconds (0 for no limit)")
                .default_value(
                    config
                        .map(ConnectionConfig::statement_timeout)
                        .filter(|seconds| *seconds > 0)
                        .map(|seconds| seconds.to_string())
                        .unwrap_or_default(),
                )
        });

        Self {
            // The form is the whole window on a first launch, and a window
            // with nothing focused has no dispatch path -- every binding is
            // dead until a field is clicked. So the field the user is meant to
            // start in asks for focus the moment it is mounted.
            needs_focus: Some(url.clone()),
            url,
            engine: config.map(ConnectionConfig::engine).unwrap_or_default(),
            name,
            path,
            host,
            port,
            database,
            user,
            password,
            sslmode: server.map(|server| server.sslmode).unwrap_or_default(),
            root_certificate,
            statement_timeout,
            error: None,
        }
    }

    fn config(&self, cx: &App) -> Result<(String, ConnectionConfig), String> {
        let read = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let name = read(&self.name);
        if name.is_empty() {
            return Err("Display name is required.".into());
        }

        let statement_timeout = self.statement_timeout(cx)?;
        let config = match self.engine {
            Engine::Sqlite => {
                let path = read(&self.path);
                if path.is_empty() {
                    return Err("Database file is required.".into());
                }
                ConnectionConfig::Sqlite {
                    path,
                    statement_timeout,
                }
            }
            Engine::Postgres => ConnectionConfig::Postgres(self.server(cx)?),
            Engine::MySql => ConnectionConfig::MySql(self.server(cx)?),
        };

        Ok((name, config))
    }

    /// Blank is 0 is no limit, so a user who never had an opinion about it is
    /// not made to have one.
    fn statement_timeout(&self, cx: &App) -> Result<u32, String> {
        let value = self.statement_timeout.read(cx).value().trim().to_string();
        if value.is_empty() {
            return Ok(0);
        }
        value
            .parse()
            .map_err(|_| "Statement timeout must be a whole number of seconds.".to_string())
    }

    fn server(&self, cx: &App) -> Result<ServerConfig, String> {
        let read = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let host = read(&self.host);
        let database = read(&self.database);
        let user = read(&self.user);
        let port = read(&self.port);

        for (label, value) in [
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

        // Kept only where it is consulted. A path left behind by switching down
        // to `require` would be stored and shown as though it were in force.
        let root_certificate = self
            .sslmode
            .checks_certificate()
            .then(|| read(&self.root_certificate))
            .filter(|path| !path.is_empty());

        Ok(ServerConfig {
            host,
            port,
            database,
            user,
            password: self.password.read(cx).unmask_value().to_string(),
            sslmode: self.sslmode,
            root_certificate,
            statement_timeout: self.statement_timeout(cx)?,
        })
    }
}

/// What a profile is called when nobody has named it: the database for an
/// engine that has one, and the file for an engine that is one.
fn default_profile_name(config: &ConnectionConfig) -> String {
    match config {
        ConnectionConfig::Postgres(server) | ConnectionConfig::MySql(server) => {
            server.database.clone()
        }
        ConnectionConfig::Sqlite { path, .. } => file_stem(path).to_string(),
    }
}

/// A database file's name without its directory or extension.
fn file_stem(path: &str) -> &str {
    std::path::Path::new(path)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(path)
}

/// What runs once a generated batch has succeeded.
enum Refresh {
    /// The query tab's stashed `SELECT`.
    Statement(String),
    /// A relation tab, refreshed the way its own controls refresh it — so it
    /// picks up whatever sort and row limit the tab is now set to.
    Relation(u64),
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
    /// Whether the explorer column is folded away. Not persisted: a hidden
    /// sidebar is a thing done for the next minute, not a preference.
    sidebar_hidden: bool,
    pending_removal: Option<String>,
    next_generation: u64,
    /// The palette, built from scratch every time it opens. Its rows are a
    /// snapshot of what the catalog held and which tab was in front, and both
    /// can move underneath it — so it is thrown away on the way out rather
    /// than kept and refreshed.
    palette: Option<Entity<ListState<Palette>>>,
    /// The window's own focus, for the moments when nothing inside it can hold
    /// any. See [`Focus::Window`].
    focus: FocusHandle,
}

impl Workspace {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut workspace = Self {
            profiles: Vec::new(),
            active: 0,
            form: None,
            switcher_open: false,
            sidebar_hidden: false,
            pending_removal: None,
            next_generation: 0,
            palette: None,
            focus: cx.focus_handle(),
        };

        let mut load_failure = None;
        match store::load_profiles() {
            Ok((profiles, active, stored_fonts)) => {
                // Before the first frame, so the window is drawn in the faces
                // the user picked rather than repainted into them.
                let available = cx.text_system().all_font_names();
                install_fonts(restored_fonts(stored_fonts, &available), cx);
                for stored in profiles {
                    workspace.restore_profile(stored, window, cx);
                }
                // Where the last session was left. An id that no longer names a
                // profile leaves the first one in front, which is where an
                // install with no history starts anyway.
                if let Some(id) = active
                    && let Some(index) = workspace
                        .profiles
                        .iter()
                        .position(|profile| profile.id == id)
                {
                    workspace.active = index;
                }
            }
            Err(message) => load_failure = Some(message),
        }

        match connection_config_from_environment() {
            Ok(Some(config)) => {
                let existing = workspace.profiles.iter().position(|profile| {
                    profile.config.engine() == config.engine()
                        && profile.config.endpoint() == config.endpoint()
                        && profile.config.server().map(|server| server.user.as_str())
                            == config.server().map(|server| server.user.as_str())
                });
                workspace.active = match existing {
                    Some(index) => index,
                    None => {
                        let name = default_profile_name(&config);
                        workspace.create_profile(name, config, Origin::Environment, window, cx)
                    }
                };
                // The environment picked the profile, so it is the one to come
                // back to next launch -- when there may be no environment.
                workspace.remember_profiles(cx);
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

        // After the form exists, because with no profiles the form is the only
        // surface a notice has.
        if let Some(message) = load_failure {
            workspace.note(message, cx);
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

    /// The engine every statement Slate generates is written for. With no
    /// profile there is nothing to run it against, so the default is only ever
    /// used to build a string nobody sends.
    fn engine(&self) -> Engine {
        self.profile()
            .map(|profile| profile.config.engine())
            .unwrap_or_default()
    }

    fn issued_to(&mut self, id: &str, generation: u64) -> Option<&mut Profile> {
        self.profiles
            .iter_mut()
            .find(|profile| profile.id == id && profile.generation == generation)
    }

    /// A notice describes what happened to the last thing the user asked for, so
    /// asking for the next thing takes it down. Otherwise a refusal like "this
    /// column cannot be edited" sits in the status bar for the rest of the
    /// session.
    ///
    /// Called from the gestures that run SQL rather than from
    /// `execute_and_then`, because a statement Slate runs on its own — restoring
    /// a tab at startup — would otherwise clear a notice nobody has read yet,
    /// and one of those says the connection came up weaker than it asked for.
    fn clear_notice(&mut self) {
        if let Some(profile) = self.profile_mut() {
            profile.session.notice = None;
        }
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
        self.set_editor_zoom(EDITOR_FONT_SIZE_DEFAULT, cx);
    }

    fn adjust_editor_zoom(&mut self, delta: f32, cx: &mut Context<Self>) {
        let Some(current) = self
            .profile()
            .map(|profile| profile.session.editor_font_size)
        else {
            return;
        };
        self.set_editor_zoom(adjusted_editor_font_size(current, delta), cx);
    }

    /// Written through to the profile, because a zoom that resets on relaunch is
    /// a setting the user has to make again every morning.
    fn set_editor_zoom(&mut self, font_size: f32, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        if profile.session.editor_font_size == font_size {
            return;
        }
        profile.session.editor_font_size = font_size;
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Written through for the same reason the zoom is: a font that resets on
    /// relaunch is a setting the user has to make again every morning.
    fn set_font(&mut self, slot: FontSlot, family: String, cx: &mut Context<Self>) {
        let mut picked = fonts(cx).clone();
        picked.set(slot, family.into());
        install_fonts(picked, cx);
        self.remember_profiles(cx);
        cx.refresh_windows();
    }

    fn remember_profiles(&mut self, cx: &mut Context<Self>) {
        let profiles = self
            .profiles
            .iter()
            .map(Profile::stored)
            .collect::<Vec<_>>();
        let active = self.profile().map(|profile| profile.id.clone());
        let picked = fonts(cx);
        let fonts = store::StoredFonts {
            chrome: Some(picked.chrome.to_string()),
            editor: Some(picked.editor.to_string()),
            grid: Some(picked.grid.to_string()),
        };
        if let Err(message) = store::save_profiles(&profiles, active.as_deref(), &fonts) {
            self.note(message, cx);
        }
    }

    fn restore_profile(
        &mut self,
        stored: store::StoredProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // No mode at all is a profile written before Slate had TLS, and
        // `prefer` is exactly what it was connecting as. A mode this build
        // cannot read is the other case, and it fails closed: whatever was
        // asked for, it was not something weaker than the strictest rung.
        let (sslmode, unreadable_mode) = match stored.sslmode.as_deref() {
            None => (SslMode::default(), None),
            Some(stored) => match SslMode::parse(stored) {
                Ok(mode) => (mode, None),
                Err(message) => (SslMode::VerifyFull, Some(message)),
            },
        };
        // No engine at all is a profile written before Slate had a second one,
        // and Postgres is what it was. An engine this build cannot read is a
        // profile written by a build that has one this one does not, so it is
        // read as Postgres and says so rather than connecting somewhere the
        // user did not ask for without mentioning it.
        let (engine, unreadable_engine) = match stored.engine.as_deref() {
            None => (Engine::Postgres, None),
            Some(stored) => match Engine::parse(stored) {
                Ok(engine) => (engine, None),
                Err(message) => (Engine::Postgres, Some(message)),
            },
        };
        let config = match engine {
            Engine::Sqlite => ConnectionConfig::Sqlite {
                path: stored.path.unwrap_or_default(),
                statement_timeout: stored.statement_timeout.unwrap_or_default(),
            },
            Engine::Postgres | Engine::MySql => {
                let server = ServerConfig {
                    host: stored.host,
                    port: stored.port,
                    database: stored.database,
                    user: stored.user,
                    // Never on disk. Read from the Keychain when connecting.
                    password: String::new(),
                    sslmode,
                    root_certificate: stored.root_certificate,
                    statement_timeout: stored.statement_timeout.unwrap_or_default(),
                };
                match engine {
                    Engine::MySql => ConnectionConfig::MySql(server),
                    _ => ConnectionConfig::Postgres(server),
                }
            }
        };
        // A profile written before a buffer was a tab carries one buffer, whose
        // name is in the legacy scalar and whose text `read_scratch` migrates.
        let stored_queries = if stored.open_queries.is_empty() {
            vec![store::StoredQueryTab {
                id: 0,
                name: stored.open_query.clone(),
                active: true,
            }]
        } else {
            stored.open_queries
        };
        // Snapshots whose tab is gone -- a renamed table strands its file
        // under the old name, and nothing else will ever remove it.
        let live_grids = stored_queries
            .iter()
            .map(|tab| store::query_grid_key(tab.id))
            .chain(
                stored
                    .open_objects
                    .iter()
                    .map(|object| store::object_grid_key(&object.schema, &object.name)),
            )
            .collect();
        store::prune_grids(&stored.id, &live_grids);
        let mut session = Session::new(
            stored.id.clone(),
            stored_queries,
            stored.next_query_id.unwrap_or(0),
            restored_editor_font_size(stored.editor_font_size),
            stored.open_objects,
            window,
            cx,
        );
        // An unreadable sslmode only means anything to an engine that has one.
        let notice = unreadable_engine
            .map(|message| format!("{message} Reading it as Postgres."))
            .or_else(|| {
                unreadable_mode
                    .filter(|_| config.server().is_some())
                    .map(|message| format!("{message} Connecting as verify-full."))
            });
        if let Some(message) = notice {
            session.notice = Some(message);
        }
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
        origin: Origin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let existing = self
            .profiles
            .iter()
            .map(|profile| profile.id.clone())
            .collect::<Vec<_>>();
        let id = store::profile_id(&name, &existing);
        let session = Session::new(
            id.clone(),
            Vec::new(),
            0,
            EDITOR_FONT_SIZE_DEFAULT,
            Vec::new(),
            window,
            cx,
        );
        let password = password_to_persist(&config, origin).map(str::to_string);
        self.profiles.push(Profile {
            id: id.clone(),
            name,
            config,
            generation: 0,
            state: ProfileState::Idle,
            catalog: CatalogState::Loading,
            session,
        });
        if let Some(password) = password
            && let Err(message) = store::set_password(&id, &password)
        {
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

        // Only the fields the URL's own engine has. Blanking the others would
        // throw away a half-typed connection to a different database, which the
        // user never asked to lose by pasting a URL.
        let filled = match &config {
            ConnectionConfig::Sqlite { path, .. } => vec![
                (&form.name, default_profile_name(&config)),
                (&form.path, path.clone()),
            ],
            ConnectionConfig::Postgres(server) | ConnectionConfig::MySql(server) => vec![
                (&form.name, server.database.clone()),
                (&form.host, server.host.clone()),
                (
                    &form.port,
                    server.port.map(|port| port.to_string()).unwrap_or_default(),
                ),
                (&form.database, server.database.clone()),
                (&form.user, server.user.clone()),
                (&form.password, server.password.clone()),
                (
                    &form.root_certificate,
                    server.root_certificate.clone().unwrap_or_default(),
                ),
            ],
        };
        for (input, value) in filled {
            let input = input.clone();
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }

        let engine = config.engine();
        let sslmode = config.server().map(|server| server.sslmode);
        if let Some(form) = &mut self.form {
            form.engine = engine;
            if let Some(sslmode) = sslmode {
                // The URL's own mode, so pasting one that demands verification
                // cannot land in a form still set to `prefer`.
                form.sslmode = sslmode;
            }
            form.error = None;
        }
        cx.notify();
    }

    /// One chip per mode, weakest first. A row of five words rather than a
    /// dropdown: the choice is the security of the connection, and it should be
    /// legible without opening anything. No keybinding, so no action type —
    /// this is only reachable while the form is on screen.
    /// One chip per engine, in the same shape as the `sslmode` row below it.
    /// The engine decides which fields the form even has, so it is the first
    /// thing on it and not a dropdown two clicks away.
    fn engine_chip(&self, engine: Engine, cx: &mut Context<Self>) -> AnyElement {
        let t = *theme(cx);
        let selected = self.form.as_ref().is_some_and(|form| form.engine == engine);
        div()
            .id(engine.as_str())
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
            .child(engine.label())
            .on_click(cx.listener(move |workspace, _, _, cx| {
                if let Some(form) = &mut workspace.form {
                    // Only when the field set actually changes: Postgres and
                    // MySQL show the same fields, so switching between them
                    // takes nothing away and must not take focus either.
                    if form.engine.is_server() != engine.is_server() {
                        form.needs_focus = Some(match engine.is_server() {
                            true => form.host.clone(),
                            false => form.path.clone(),
                        });
                    }
                    form.engine = engine;
                    // The error belonged to the fields that just left the
                    // screen, so it would be reporting something invisible.
                    form.error = None;
                    cx.notify();
                }
            }))
            .into_any_element()
    }

    fn sslmode_chip(&self, mode: SslMode, cx: &mut Context<Self>) -> AnyElement {
        let t = *theme(cx);
        let selected = self.form.as_ref().is_some_and(|form| form.sslmode == mode);
        div()
            .id(mode.as_str())
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
            .child(mode.label())
            .on_click(cx.listener(move |workspace, _, _, cx| {
                if let Some(form) = &mut workspace.form {
                    // Stepping down from a verifying mode unmounts the
                    // certificate field, which may be the one holding focus.
                    if form.sslmode.checks_certificate() && !mode.checks_certificate() {
                        form.needs_focus = Some(form.password.clone());
                    }
                    form.sslmode = mode;
                    cx.notify();
                }
            }))
            .into_any_element()
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
        let index = self.create_profile(name, config, Origin::Form, window, cx);
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
                // A file engine has nothing to authenticate to, so it never
                // reaches the Keychain — and never triggers its prompt.
                if let Some(server) = config.server_mut()
                    && server.password.is_empty()
                {
                    match store::password(&id) {
                        Ok(Some(password)) => server.password = password,
                        // No keychain item is not a missing password: a blank
                        // one is valid, so this connects with what it has.
                        Ok(None) => {}
                        Err(message) => return Err(message),
                    }
                }
                Connection::open(config).map_err(|error| error.message)
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
                        Err(message) => ProfileState::Failed(message),
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
                    // Relations restore before the catalog arrives, wearing
                    // whatever kind was on disk -- a default, for a profile an
                    // older build wrote. This is the first moment there is
                    // anything to correct it from.
                    if let CatalogState::Loaded(catalog) = &profile.catalog {
                        for tab in &mut profile.session.objects {
                            if let ObjectKind::Relation(kind) = &mut tab.kind
                                && let Some(actual) = relation_kind(catalog, &tab.schema, &tab.name)
                            {
                                *kind = actual;
                            }
                        }
                    }
                    workspace.install_completions(&id, cx);
                    workspace.refresh_explorer(&id, cx);
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Point this profile's editor at what its catalog now holds.
    ///
    /// The provider is replaced whole rather than kept and mutated: a catalog
    /// arrives as one value and is never patched, so a snapshot behind an `Rc`
    /// needs no interior mutability and cannot be half-updated. Anything but a
    /// loaded catalog leaves the editor with no provider at all, which is the
    /// difference between offering nothing and offering the last database's
    /// tables to a buffer written against this one.
    /// Fetch one relation's columns for the completion cache.
    ///
    /// Reuses `Connection::structure`, which the Structure tab already runs, so
    /// completion adds no SQL of its own to any engine. It asks for more than
    /// it needs -- indexes and constraints come back too -- and that is the
    /// trade: one extra pair of catalog queries per relation the session
    /// actually writes about, against three more engine-specific statements to
    /// maintain and keep in step with hard rule 4.
    fn load_completion_columns(
        &mut self,
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
        let id = profile.id.clone();
        let generation = profile.generation;
        let columns = profile.session.completion_columns.clone();

        let task = cx.background_executor().spawn({
            let (schema, relation) = (schema.clone(), relation.clone());
            async move { connection.structure(&schema, &relation) }
        });
        cx.spawn(async move |workspace, cx| {
            let result = task.await;
            workspace
                .update(cx, |workspace, cx| {
                    // A reconnect clears the cache and starts a new generation,
                    // so a result from the old one describes a database this
                    // profile is no longer talking to.
                    if workspace.issued_to(&id, generation).is_none() {
                        return;
                    }
                    let key = (schema, relation);
                    let state = match result {
                        Ok(structure) => completion::ColumnState::Loaded(
                            structure
                                .columns
                                .into_iter()
                                .map(|column| column.name)
                                .collect(),
                        ),
                        // The attempt count rides on the `Loading` the request
                        // wrote, so a relation that keeps failing runs out.
                        Err(_) => {
                            let attempts = match columns.borrow().get(&key) {
                                Some(completion::ColumnState::Loading(attempts)) => *attempts,
                                _ => 0,
                            };
                            completion::ColumnState::Failed(attempts.saturating_add(1))
                        }
                    };
                    columns.borrow_mut().insert(key, state);
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn install_completions(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter().find(|profile| profile.id == id) else {
            return;
        };
        // The catalog is new, so what was known about any relation's columns
        // describes a schema that may no longer exist.
        profile.session.completion_columns.borrow_mut().clear();

        let provider = match &profile.catalog {
            CatalogState::Loaded(catalog) => Some(Rc::new(SchemaCompletions::new(
                Arc::new(catalog.clone()),
                profile.session.completion_columns.clone(),
                cx.weak_entity(),
            )) as Rc<dyn CompletionProvider>),
            _ => None,
        };

        // Every buffer, not just the one in front: a tab switch must not be a
        // moment where completion quietly stops working.
        for tab in &profile.session.queries {
            tab.editor.update(cx, |editor, _| {
                editor.lsp.completion_provider = provider.clone();
            });
        }
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
        // Written here rather than at quit, so the profile in front survives a
        // crash as well as a close.
        self.remember_profiles(cx);
        if let Some(profile) = self.profile_mut() {
            profile.session.editor_needs_focus = true;
            profile.session.clear_prompts();
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

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_hidden = !self.sidebar_hidden;
        // A folded sidebar takes the open switcher panel with it: the panel is
        // anchored to a row that is no longer on screen.
        self.switcher_open = false;
        cx.notify();
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

        // Counted before the directory goes, because afterwards there is
        // nothing left to count and the number is what the note reports.
        let queries = store::saved_queries(&id).len();

        self.profiles.remove(index);
        store::delete_password(&id);
        let removed_queries = store::delete_queries(&id);
        let _ = store::delete_grids(&id);
        self.pending_removal = None;
        self.active = active_after_removal(self.active, index, self.profiles.len());
        self.remember_profiles(cx);
        if self.profiles.is_empty() {
            self.form = Some(ConnectionForm::new(None, window, cx));
        } else {
            self.connect_active(cx);
            self.note(removal_note(&name, queries, removed_queries.err()), cx);
        }
        cx.notify();
    }

    fn open_explorer_target(
        &mut self,
        target: ExplorerTarget,
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
            && let Some(id) = self.open_object(opened, window, cx)
        {
            self.activate_tab(Tab::Object(id), cx);
            self.remember_profiles(cx);
        }
    }

    /// Give an object a tab, reusing the one it already has. Opening does not
    /// show it — the caller decides that, so restoring a session can rebuild
    /// six tabs without running six queries.
    fn open_object(
        &mut self,
        opened: OpenedObject,
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
            return Some(id);
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
                limit: PREVIEW_ROW_LIMIT,
                stale: false,
                hydrated: false,
            },
        };
        self.profile_mut()?.session.objects.push(ObjectTab {
            id,
            schema,
            name,
            kind,
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
        let ObjectBody::Relation { query, stale, .. } = &tab.body else {
            return;
        };
        // A tab restored from a snapshot is `Complete` over rows nothing has
        // checked against the server, so it gets exactly one run -- and its
        // structure, which no snapshot keeps.
        if !*stale && !matches!(query, QueryState::Idle | QueryState::Failed(_)) {
            return;
        }

        let (schema, relation) = (tab.schema.clone(), tab.name.clone());
        self.load_structure(id, schema, relation, cx);
        self.requery_relation(id, |_, _| true, cx);
    }

    /// Run a relation tab's statement again, after `change` has had its say
    /// about the tab's sort and row limit. `false` from `change` means nothing
    /// moved, and nothing runs.
    ///
    /// Every path that re-queries a relation comes through here. The statement
    /// is Slate's own, so it is regenerated from whatever the tab is now set to
    /// rather than edited — the row limit and the quoting cannot drift out of
    /// one place — and `QueryState::Idle` is what makes a preview willing to run
    /// again, so no caller can forget it.
    fn requery_relation(
        &mut self,
        id: u64,
        change: impl FnOnce(&mut Vec<SortKey>, &mut usize) -> bool,
        cx: &mut Context<Self>,
    ) {
        let engine = self.engine();
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(tab) = profile.session.objects.iter_mut().find(|tab| tab.id == id) else {
            return;
        };
        let (schema, relation) = (tab.schema.clone(), tab.name.clone());
        let ObjectBody::Relation {
            sort,
            query,
            limit,
            stale,
            ..
        } = &mut tab.body
        else {
            return;
        };
        if !change(sort, limit) {
            return;
        }

        // The one run that must not blank the grid first: a restored tab's rows
        // are the rows it was showing, and clearing them to fetch the same
        // thing again is a flash of nothing. Taken here rather than tested,
        // because every later run is replacing rows the server sent and has to
        // clear them.
        let keep_rows = std::mem::take(stale);
        let sql = relation_sql(engine, &schema, &relation, sort, *limit);
        // A preview only re-queries when it is asked to, and this is the ask.
        *query = QueryState::Idle;
        self.execute_and_then(sql, Tab::Object(id), None, keep_rows, cx);
    }

    /// A header click on a relation tab: move that column through the sort and
    /// ask the server again.
    fn relation_sort(&mut self, id: u64, column: usize, cx: &mut Context<Self>) {
        let engine = self.engine();
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some((_, results)) = profile.session.slot(Tab::Object(id)) else {
            return;
        };
        let Some(expression) =
            sort_expression(engine, results.read(cx).delegate().columns(), column)
        else {
            return;
        };
        self.requery_relation(
            id,
            move |sort, _| {
                cycle(sort, &expression);
                true
            },
            cx,
        );
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
        let key = (schema.clone(), relation.clone());
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
                    // The same call completion makes, so completion should not
                    // make it again for this relation.
                    if let Ok(structure) = &result {
                        profile.session.completion_columns.borrow_mut().insert(
                            key,
                            completion::ColumnState::Loaded(
                                structure
                                    .columns
                                    .iter()
                                    .map(|column| column.name.clone())
                                    .collect(),
                            ),
                        );
                    }
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
            && let ObjectBody::Relation {
                showing_structure: showing,
                ..
            } = &mut tab.body
        {
            *showing = showing_structure;
            cx.notify();
        }
    }

    /// Put a tab's snapshot on screen, the first time the tab is looked at.
    ///
    /// Startup used to parse every stored tab's snapshot, which is a megabyte
    /// of JSON per handful of tabs decoded inside `Render` for grids nobody has
    /// asked to see. A tab that is never reached now touches the disk not at
    /// all.
    ///
    /// Nothing here can land on a live result. `hydrated` is set on the first
    /// attempt whether or not a snapshot was found, so the read happens at most
    /// once per tab; and a tab whose state is anything but `Idle` has a run of
    /// its own -- in flight, finished or failed -- so it is left alone even on
    /// that one attempt.
    fn hydrate_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let profile_id = profile.id.clone();
        let key = match tab {
            Tab::Query(id) => {
                let Some(query_tab) = profile.session.query_tab_mut(id) else {
                    return;
                };
                if std::mem::replace(&mut query_tab.hydrated, true)
                    || !matches!(query_tab.query, QueryState::Idle)
                {
                    return;
                }
                store::query_grid_key(id)
            }
            Tab::Object(id) => {
                let Some(object) = profile.session.objects.iter_mut().find(|tab| tab.id == id)
                else {
                    return;
                };
                let key = store::object_grid_key(&object.schema, &object.name);
                let ObjectBody::Relation {
                    query, hydrated, ..
                } = &mut object.body
                else {
                    return;
                };
                if std::mem::replace(hydrated, true) || !matches!(query, QueryState::Idle) {
                    return;
                }
                key
            }
        };

        let Some(snapshot) = store::read_grid(&profile_id, &key) else {
            return;
        };
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.results(tab))
            .cloned()
        else {
            return;
        };
        show_snapshot(&results, &snapshot, cx);
        let Some(profile) = self.profile_mut() else {
            return;
        };
        match tab {
            // A query tab's rows are all a snapshot restores: the statement
            // behind them is arbitrary SQL the user wrote, so nothing re-runs
            // it until they ask. It could be an `UPDATE ... RETURNING`.
            Tab::Query(id) => {
                if let Some(query_tab) = profile.session.query_tab_mut(id) {
                    query_tab.query = restored_state(&snapshot);
                    query_tab.last_query = snapshot.last_query;
                }
            }
            Tab::Object(id) => {
                if let Some(object) = profile.session.objects.iter_mut().find(|tab| tab.id == id)
                    && let ObjectBody::Relation {
                        showing_structure,
                        query,
                        sort,
                        limit,
                        stale,
                        ..
                    } = &mut object.body
                {
                    *showing_structure = snapshot.showing_structure;
                    *query = restored_state(&snapshot);
                    // The sort the snapshot's rows are actually in, so the
                    // refresh asks for the same order rather than whatever the
                    // server hands back unordered.
                    *sort = snapshot
                        .order_by
                        .iter()
                        .map(|(expression, ascending)| SortKey::new(expression.clone(), *ascending))
                        .collect();
                    *limit = snapshot.limit.unwrap_or(PREVIEW_ROW_LIMIT);
                    *stale = true;
                }
            }
        }
    }

    fn activate_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut() {
            profile.session.active = tab;
            profile.session.clear_prompts();
            profile.session.editor_needs_focus = true;
        }
        // Before `load_relation`, which decides whether to re-query from the
        // state the snapshot leaves the tab in: hydrating afterwards would
        // arrive over a run already in flight and be refused.
        self.hydrate_tab(tab, cx);
        if let Tab::Object(id) = tab {
            self.load_relation(id, cx);
        }
        self.remember_profiles(cx);
        cx.notify();
    }

    fn close_object(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut() {
            // Read before the tab goes, because the key is made of its schema
            // and name and there is nothing left to make it from afterwards.
            let snapshot = profile
                .session
                .objects
                .iter()
                .find(|tab| tab.id == id)
                .map(|tab| store::object_grid_key(&tab.schema, &tab.name));
            let profile_id = profile.id.clone();
            profile.session.objects.retain(|tab| tab.id != id);
            if let Some(key) = snapshot {
                let _ = store::remove_grid(&profile_id, &key);
            }
            if profile.session.active == Tab::Object(id)
                && let Some(first) = profile.session.queries.first().map(|tab| tab.id)
            {
                profile.session.active = Tab::Query(first);
                profile.session.editor_needs_focus = true;
            }
        }
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Turn the object tabs read back from disk into live ones. A relation is
    /// opened as soon as there is a connection to query, because everything its
    /// tab needs is on disk; only a routine, whose body the tab renders, waits
    /// for the catalog. Anything the database no longer has simply does not
    /// come back -- a relation it has dropped comes back as a tab whose query
    /// fails, which says so where silence did not.
    fn restore_objects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let pending = profile.session.pending_objects.len();
        if pending == 0 {
            return;
        }
        let connected = profile.connection().is_some();
        let catalog = match &profile.catalog {
            CatalogState::Loaded(catalog) => Some(catalog),
            _ => None,
        };

        let mut opened = Vec::new();
        let mut still_pending = Vec::new();
        for stored in &profile.session.pending_objects {
            if stored.routine {
                match catalog {
                    Some(catalog) => opened.extend(
                        OpenedObject::resolve(catalog, stored)
                            .map(|object| (object, stored.active)),
                    ),
                    None => still_pending.push(stored.clone()),
                }
            } else if connected {
                opened.push((
                    OpenedObject::Relation {
                        schema: stored.schema.clone(),
                        name: stored.name.clone(),
                        // The catalog when it is here, because `stored.kind` can
                        // be the default a build that did not keep one wrote --
                        // which draws every view with a table's icon.
                        kind: catalog
                            .and_then(|catalog| {
                                relation_kind(catalog, &stored.schema, &stored.name)
                            })
                            .unwrap_or(stored.kind),
                    },
                    stored.active,
                ));
            } else {
                still_pending.push(stored.clone());
            }
        }
        // Called on every frame, so a pass that could do nothing has to change
        // nothing: rewriting the profile here would write to disk per frame.
        if opened.is_empty() && still_pending.len() == pending {
            return;
        }

        if let Some(profile) = self.profile_mut() {
            profile.session.pending_objects = still_pending;
        }
        let mut restored_active = None;
        for (opened, active) in opened {
            let id = self.open_object(opened, window, cx);
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
    fn cycle_theme(&mut self, _: &CycleTheme, window: &mut Window, cx: &mut Context<Self>) {
        let next = theme(cx).next();
        next.apply_to_components(cx);
        cx.set_global(next);
        // Only a glass theme wants the desktop behind it, and the platform
        // tears the vibrant view out of the window the moment this says
        // otherwise -- so it has to be said again on every switch, not once at
        // startup.
        window.set_background_appearance(next.window_background());
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
        // Whatever is in front, in the order it is stacked: the palette is over
        // the batch panel, which is over the surface, so `escape` backs out of
        // them one at a time.
        if self.close_palette(cx) {
            return;
        }
        if self.cancel_discard_close(cx) {
            return;
        }
        if self.cancel_close_tab(cx) {
            return;
        }
        if self.close_apply_review(cx) {
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
        if matches!(profile.session.active, Tab::Query(_)) {
            // Nothing of Slate's is stacked over the editor, so the keystroke
            // is not ours. Handing it on is what lets the completion popup --
            // which is the input's, not Slate's -- close on `escape`; this
            // binding is unscoped and would otherwise win it at every depth.
            cx.propagate();
            return;
        }
        let Some(&QueryTab { id, .. }) = profile.session.queries.first() else {
            return;
        };
        profile.session.active = Tab::Query(id);
        profile.session.editor_needs_focus = true;
        self.remember_profiles(cx);
        cx.notify();
    }

    /// `tab` takes the highlighted suggestion, and indents when there is none
    /// to take.
    fn accept_completion(
        &mut self,
        _: &AcceptCompletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self
            .profile()
            .and_then(|profile| profile.session.editor(profile.session.active))
        else {
            return;
        };
        let accepted = editor.update(cx, |editor, cx| {
            editor.handle_action_for_context_menu(Box::new(Enter { secondary: false }), window, cx)
        });
        if !accepted {
            window.dispatch_action(Box::new(IndentInline), cx);
        }
    }

    /// `cmd+w` on whatever surface is in front.
    ///
    /// An object tab closes: it is a view onto something the database still
    /// holds, and reopening it costs a click. A saved query is a file, and
    /// closing its tab is deleting that file — the strip has no room for a
    /// query that exists but is not listed — so that one asks first. The
    /// scratch buffer has no closed state at all and is left alone.
    fn close_tab(&mut self, _: &CloseTab, _: &mut Window, cx: &mut Context<Self>) {
        // The palette is over the tab and holds the keyboard: a stroke that
        // reached here through it would close a tab nobody was looking at.
        if self.palette.is_some() {
            return;
        }
        let Some(profile) = self.profile() else {
            return;
        };
        let session = &profile.session;
        let unsaved = session
            .queries
            .iter()
            .filter(|tab| tab.open_query.is_none())
            .count();
        let Some(target) = close_target(session.active, session.open_query(), unsaved) else {
            return;
        };
        self.ask_before_close(target, cx);
    }

    /// Close a tab, asking first if it holds cell edits nobody has applied.
    ///
    /// Every gesture that takes a tab away comes through here -- `cmd+w`, the
    /// chip's own close button, the palette -- because the edits are lost the
    /// same way whichever one it was, and a guard on one path is a guard on
    /// none.
    fn ask_before_close(&mut self, target: CloseTarget, cx: &mut Context<Self>) {
        let unapplied = self.profile().is_some_and(|profile| {
            target
                .tab(&profile.session)
                .and_then(|tab| profile.session.results(tab))
                .is_some_and(|results| results.read(cx).delegate().has_pending())
        });
        if !unapplied {
            self.close_now(target, cx);
            return;
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.pending_discard = Some(target);
        }
        cx.notify();
    }

    /// Carry out a close that has been decided on. A saved query asks its own
    /// question from here: closing its tab deletes its file.
    fn close_now(&mut self, target: CloseTarget, cx: &mut Context<Self>) {
        match target {
            CloseTarget::Object(id) => self.close_object(id, cx),
            CloseTarget::Buffer(id) => self.close_buffer(id, cx),
            CloseTarget::SavedQuery(name) => {
                if let Some(profile) = self.profile_mut() {
                    profile.session.pending_close = Some(name);
                }
                cx.notify();
            }
        }
    }

    /// Close the tab the discard prompt was raised over, edits and all.
    fn confirm_discard_close(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self
            .profile_mut()
            .and_then(|profile| profile.session.pending_discard.take())
        else {
            return;
        };
        self.close_now(target, cx);
    }

    fn cancel_discard_close(&mut self, cx: &mut Context<Self>) -> bool {
        let cancelled = self
            .profile_mut()
            .and_then(|profile| profile.session.pending_discard.take())
            .is_some();
        if cancelled {
            cx.notify();
        }
        cancelled
    }

    fn cancel_close_tab(&mut self, cx: &mut Context<Self>) -> bool {
        let cancelled = self
            .profile_mut()
            .and_then(|profile| profile.session.pending_close.take())
            .is_some();
        if cancelled {
            cx.notify();
        }
        cancelled
    }

    fn fuzzy_open(&mut self, _: &FuzzyOpen, window: &mut Window, cx: &mut Context<Self>) {
        self.open_palette(PaletteMode::Jump, window, cx);
    }

    fn command_palette(&mut self, _: &CommandPalette, window: &mut Window, cx: &mut Context<Self>) {
        self.open_palette(PaletteMode::Commands, window, cx);
    }

    /// The same stroke again closes the palette; the other one swaps which list
    /// it is showing, so the two surfaces are one keystroke apart.
    fn open_palette(&mut self, mode: PaletteMode, window: &mut Window, cx: &mut Context<Self>) {
        let showing = self
            .palette
            .as_ref()
            .map(|list| list.read(cx).delegate().mode());
        if showing == Some(mode) || self.profile().is_none() {
            self.close_palette(cx);
            return;
        }

        let palette = Palette::new(mode, self, cx);
        let list = cx.new(|cx| ListState::new(palette, window, cx).searchable(true));
        // Nothing is selected on a fresh list, and `enter` on nothing selected
        // does nothing -- so the first row is chosen before it is ever drawn.
        list.update(cx, |list, cx| {
            list.set_selected_index(Some(IndexPath::default()), window, cx);
        });
        cx.subscribe_in(&list, window, Self::on_palette_event)
            .detach();
        self.palette = Some(list);
        cx.notify();
    }

    /// The palette is dismissed before its command runs, always. A command can
    /// open a tab, a form or a modal, and none of them can come up underneath
    /// an overlay that is still holding the keyboard.
    fn on_palette_event(
        &mut self,
        list: &Entity<ListState<Palette>>,
        event: &ListEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = match event {
            ListEvent::Select(_) => return,
            ListEvent::Cancel => None,
            ListEvent::Confirm(index) => list.read(cx).delegate().command(index.row).cloned(),
        };
        self.close_palette(cx);
        if let Some(command) = command {
            self.run_command(command, window, cx);
        }
    }

    /// Take the palette down and hand the keyboard back.
    ///
    /// Handing it back is the whole job. The palette's search field is what had
    /// focus, and it goes with the palette — leaving the window focused on
    /// nothing, with no dispatch path, and every binding dead until something
    /// is clicked. Including the one that would reopen the palette.
    ///
    /// Every way out routes through here for that reason: `escape`, the same
    /// stroke again, a click outside, and confirming a row.
    fn close_palette(&mut self, cx: &mut Context<Self>) -> bool {
        if self.palette.take().is_none() {
            return false;
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.editor_needs_focus = true;
        }
        cx.notify();
        true
    }

    /// Every row runs through the method its button or keystroke already calls.
    /// The palette is another way in, never a second implementation.
    fn run_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            Command::OpenObject(target) => self.open_explorer_target(target, window, cx),
            Command::OpenQuery(name) => self.open_saved_query(name, window, cx),
            Command::OpenScratch => self.open_scratch_query(window, cx),
            Command::NewQuery => self.new_query(&NewQuery, window, cx),
            Command::RunQuery => self.run_query(&RunQuery, window, cx),
            Command::SaveQuery => self.save_query(&SaveQuery, window, cx),
            Command::RenameQuery => self.rename_query(window, cx),
            Command::QueryHistory => self.open_palette(PaletteMode::History, window, cx),
            Command::RecallStatement(sql) => self.recall_statement(sql, window, cx),
            Command::ShowStructure(showing) => self.show_structure(showing, cx),
            Command::RefreshRelation(id) => self.refresh_relation(id, cx),
            Command::CloseObject(id) => self.ask_before_close(CloseTarget::Object(id), cx),
            Command::ApplyEdits => self.apply_edits(&ApplyEdits, window, cx),
            Command::DiscardEdits => self.discard_edits(&DiscardEdits, window, cx),
            Command::ExportResults(format) => self.export_results(format, cx),
            Command::SwitchProfile(index) => self.activate(index, cx),
            Command::NewConnection => self.open_connection_form(&NewConnection, window, cx),
            Command::CycleTheme => self.cycle_theme(&CycleTheme, window, cx),
            Command::PickFont(slot) => self.open_palette(PaletteMode::Font(slot), window, cx),
            Command::SetFont(slot, family) => self.set_font(slot, family, cx),
            Command::ToggleSidebar => self.toggle_sidebar(&ToggleSidebar, window, cx),
            Command::ResetEditorZoom => self.reset_editor_zoom(&ResetEditorZoom, window, cx),
        }
    }

    fn palette_next(&mut self, _: &PaletteNext, window: &mut Window, cx: &mut Context<Self>) {
        self.move_palette_selection(1, window, cx);
    }

    fn palette_previous(
        &mut self,
        _: &PalettePrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_palette_selection(-1, window, cx);
    }

    /// The list binds the arrows itself, but the search field is deeper in the
    /// dispatch path than the list is, and a single-line input swallows them
    /// without passing them on. So the palette moves its own selection.
    fn move_palette_selection(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(list) = self.palette.clone() else {
            return;
        };
        list.update(cx, |list, cx| {
            let rows = list.delegate().len() as isize;
            if rows == 0 {
                return;
            }
            let row = list.selected_index().map_or(0, |index| index.row) as isize;
            // Wrapping, because a list this short is faster to reach the end of
            // from the top than by holding a key down.
            let row = (row + step).rem_euclid(rows) as usize;
            list.set_selected_index(Some(IndexPath::new(row)), window, cx);
            list.scroll_to_selected_item(window, cx);
        });
    }

    /// Edits sitting in the visible grid, waiting to be written back. Read off
    /// the grid rather than held anywhere, so nothing can disagree with the
    /// cells about whether there is something to apply.
    fn has_pending_edits(&self, cx: &App) -> bool {
        self.profile().is_some_and(|profile| {
            profile
                .session
                .active_results()
                .is_some_and(|results| results.read(cx).delegate().has_pending())
        })
    }

    /// Whether the surface in front has a result set to write out. Columns, not
    /// rows: a statement that matched nothing still has a shape, and a
    /// header-only CSV is a truthful answer to it.
    fn has_results(&self, cx: &App) -> bool {
        self.profile().is_some_and(|profile| {
            profile
                .session
                .active_results()
                .is_some_and(|results| !results.read(cx).delegate().result().columns.is_empty())
        })
    }

    /// Ask the server to stop whatever the active profile is running.
    ///
    /// Nothing is marked cancelled here. The statement is still in flight until
    /// the driver returns, and what it returns — rows, or the server's own word
    /// for having been stopped — is what the surface shows, through the same
    /// completion every other run goes through.
    ///
    /// On the background executor because the Postgres path opens a socket and
    /// spins a current-thread tokio runtime inside `cancel_query` to do it.
    /// That is legal for exactly the reason connecting is (AGENTS.md, "Do not
    /// add tokio"): the runtime belongs to the blocking driver and lives and
    /// dies on the thread the driver is running on. Nothing tokio-shaped is
    /// handed to GPUI's executor, which is the thing that panics.
    fn cancel_query(&mut self, _: &CancelQuery, _: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.profile().and_then(Profile::connection) else {
            return;
        };
        let cancel_task = cx
            .background_executor()
            .spawn(async move { connection.cancel() });

        cx.spawn(async move |workspace, cx| {
            if let Err(error) = cancel_task.await {
                _ = workspace.update(cx, |workspace, cx| workspace.note(error.message, cx));
            }
        })
        .detach();
    }

    fn run_query(&mut self, _: &RunQuery, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_notice();
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
        self.clear_notice();
        self.requery_relation(id, |_, _| true, cx);
    }

    /// Ask a relation's preview for a different number of rows.
    ///
    /// The cap is the point of the row limit, so this moves it rather than
    /// removing it: a `SELECT` with no limit at all is what the query tab is
    /// for, where the statement is the user's and its cost is theirs to judge.
    fn set_row_limit(&mut self, action: &SetRowLimit, _: &mut Window, cx: &mut Context<Self>) {
        let rows = action.rows;
        self.clear_notice();
        let Some(Tab::Object(id)) = self.profile().map(|profile| profile.session.active) else {
            return;
        };
        self.requery_relation(
            id,
            move |_, limit| {
                let moved = *limit != rows;
                *limit = rows;
                moved
            },
            cx,
        );
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
        self.clear_notice();
        let Some(profile) = self.profile() else {
            return;
        };

        match profile.session.active {
            Tab::Object(id) => self.relation_sort(id, column, cx),
            Tab::Query(_) => self.query_sort(column, window, cx),
        }
    }

    /// Sorting a query the user wrote: the `ORDER BY` goes into their statement,
    /// where they can see it, edit it and undo it. Slate changes SQL only when
    /// asked, and a header click is the ask (`AGENTS.md`, rule 1).
    fn query_sort(&mut self, column: usize, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine();
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(tab) = profile.session.active_query_tab() else {
            return;
        };
        let tab_id = tab.id;
        let editor = tab.editor.clone();
        let results = tab.results.clone();

        let Some(expression) =
            sort_expression(engine, results.read(cx).delegate().columns(), column)
        else {
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
        self.execute_sql(sorted, Tab::Query(tab_id), cx);
    }

    /// `Enter` on the active cell opens an input on it. Everything after this
    /// keystroke — the input, the commit, the cancel — belongs to the grid; the
    /// refusal belongs here, because the grid has nowhere to say anything.
    fn edit_cell(&mut self, _: &EditCell, _: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        // A batch already on screen was generated from the pending set as it
        // stood. Another edit behind the modal would leave the statement the
        // user is reading describing something other than what the grid holds.
        if profile.session.apply_review.is_some() {
            return;
        }
        let Some(results) = profile.session.active_results().cloned() else {
            return;
        };
        // The grid's own active cell, not the library's selection: its selected
        // row and column are mutually exclusive modes rather than a cell, so
        // `selected_col` is `None` after a click. Both of its selections are
        // folded into this coordinate, which is why arrow keys land here too.
        let Some((row, col)) = results.read(cx).delegate().active() else {
            return;
        };
        if results.update(cx, |table, cx| {
            let opened = table.delegate_mut().begin_edit(row, col);
            cx.notify();
            opened
        }) {
            return;
        }

        // Which of the two refusals this is, read off `editable` rather than off
        // an edit target the grid deliberately does not expose: a result Slate
        // cannot trace to one table has no editable cell anywhere in the row,
        // and one it can has this column alone refused.
        let traced = {
            let table = results.read(cx);
            (0..table.delegate().columns().len()).any(|col| table.delegate().editable(row, col))
        };
        self.note(
            match traced {
                true => "This column cannot be edited.".into(),
                false => "Slate cannot tell which table these rows come from.".into(),
            },
            cx,
        );
    }

    /// `cmd+c` on the active cell, whole value and all.
    ///
    /// The one-cell answer beside `export_results`' whole-grid one, and still
    /// the common case: reaching for a file to carry a single value across is
    /// the long way round. It works on every cell, including the ones that can never
    /// open an input — a join, an aggregate, a view, a primary-key column — and
    /// the grid withholds the value while an input is open, where `cmd+c`
    /// belongs to the input's own text selection.
    fn copy_cell(&mut self, _: &CopyCell, _: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(results) = profile.session.active_results() else {
            return;
        };
        let Some(value) = results
            .read(cx)
            .delegate()
            .active_value()
            .map(str::to_string)
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(value));
    }

    /// Write the result set in front of the user to a file they pick.
    ///
    /// The rows on screen and only those. A relation tab holds what its
    /// row-limit chip asked for, and an export that quietly re-fetched the whole
    /// table behind that chip would make the number on it a lie — the tab has no
    /// buffer to show a larger statement in, so nothing would be on screen to
    /// read it off. Pending edits are not written either: this is the result set
    /// the server returned, and applying them is a separate, visible act.
    fn export_results(&mut self, format: Format, cx: &mut Context<Self>) {
        // A restored snapshot holds at most `GRID_ROW_CAP` rows of a larger
        // result. Exporting those is a file that is quietly short of what the
        // status bar says the tab is showing, so it refuses and says how to get
        // the rest -- rather than writing a wrong file successfully.
        let capped = self.profile().and_then(|profile| {
            let grid = profile.session.active_results()?.read(cx).delegate();
            let showing = grid.result().rows.len();
            (showing < grid.total_rows()).then(|| (showing, grid.total_rows()))
        });
        if let Some((showing, total)) = capped {
            self.note(
                format!(
                    "This tab is showing {} of {} rows from a snapshot. Refresh it before exporting.",
                    group_thousands(showing as u64),
                    group_thousands(total as u64)
                ),
                cx,
            );
            return;
        }
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(results) = profile.session.active_results() else {
            return;
        };
        // ponytail: the whole result set is copied here, on the frame thread,
        // before the panel even opens -- a wasted copy if the user cancels. It
        // is taken now rather than after the await because the panel is
        // modeless: what the user was looking at when they asked is the only
        // unambiguous answer to what they asked to export. `ResultGrid` holding
        // its `QueryResult` behind an `Arc` is the upgrade path if a large
        // export is ever seen to stutter.
        let result = results.read(cx).delegate().result().clone();
        if result.columns.is_empty() {
            return;
        }

        // What the tab calls itself, so the file lands named after the thing the
        // user was looking at rather than after the statement that built it.
        let stem = match profile.session.active_object() {
            Some(tab) => tab.name.clone(),
            None => profile
                .session
                .open_query()
                .map(str::to_string)
                .unwrap_or_else(|| "results".to_string()),
        };
        let id = profile.id.clone();
        let generation = profile.generation;
        let rows = result.rows.len();
        let suggested = format!("{stem}.{}", format.extension());
        let directory = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let chosen = cx.prompt_for_new_path(&directory, Some(&suggested));

        cx.spawn(async move |workspace, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            // Rendering a hundred thousand rows is not work to do on the frame
            // thread, and the write even less so.
            let written = cx
                .background_executor()
                .spawn(async move {
                    // The extension decides the format, not the row that started
                    // this: someone who typed `.json` over the suggested `.csv`
                    // asked for JSON. Which is why the notice says which one it
                    // wrote -- that rename is the one thing they could have got
                    // wrong, and the file name alone does not read it back.
                    let format = Format::for_path(&path);
                    let text = export::render(format, &result);
                    match std::fs::write(&path, text) {
                        Ok(()) => Ok((path, format)),
                        Err(error) => Err(format!("Could not write {}: {error}", path.display())),
                    }
                })
                .await;
            _ = workspace.update(cx, |workspace, cx| {
                // The panel is modeless, so the profile can have moved under
                // this task while it was open. `note` writes to whichever
                // profile is active now, which would announce an export in a
                // session that never ran the query behind it.
                let Some(profile) = workspace.issued_to(&id, generation) else {
                    return;
                };
                profile.session.notice = Some(match written {
                    Ok((path, format)) => format!(
                        "Exported {} {} as {} to {}.",
                        group_thousands(rows as u64),
                        if rows == 1 { "row" } else { "rows" },
                        format.extension().to_uppercase(),
                        path.display()
                    ),
                    Err(error) => error,
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Turn the grid's pending edits into SQL and put it where the user can read
    /// it: the query tab's buffer, or a relation tab's modal.
    ///
    /// Deliberately not on a keybinding. `cmd+enter` means "run the statement
    /// under the cursor" and nothing else, and a mutation one fat finger away
    /// from that is a write nobody asked for.
    fn apply_edits(&mut self, _: &ApplyEdits, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let tab = profile.session.active;
        let Some(results) = profile.session.active_results().cloned() else {
            return;
        };
        let pending = results.read(cx).delegate().pending_updates();
        let Some(batch) = update_batch(self.engine(), &pending) else {
            self.note(
                match pending.is_empty() {
                    true => "There are no edits to apply.".into(),
                    // Nothing partial runs: a batch missing one of its rows is
                    // not the change the user made.
                    false => "Slate cannot name an edited row by its primary key.".into(),
                },
                cx,
            );
            return;
        };
        // The gate every generated statement passes before anything executes
        // (`AGENTS.md` rule 2). Failing it means Slate wrote something that is
        // not an `UPDATE`, which is a bug in Slate rather than a user error.
        if !sql::is_generated_update(&batch) {
            self.note(
                "Slate refused to run a statement it wrote itself: it is not an UPDATE.".into(),
                cx,
            );
            return;
        }

        match tab {
            Tab::Query(_) => self.apply_in_buffer(batch, window, cx),
            Tab::Object(_) => {
                if let Some(profile) = self.profile_mut() {
                    profile.session.apply_review = Some(ApplyReview { tab, sql: batch });
                    cx.notify();
                }
            }
        }
    }

    /// The query tab: the batch is appended to the user's buffer, runs from
    /// there, and the `SELECT` that produced the grid runs after it.
    ///
    /// The append is what keeps a failure readable. `execute_sql` clears the
    /// grid as it starts, so the pending edits are gone either way — but the
    /// statement that was attempted is still in the buffer.
    fn apply_in_buffer(&mut self, batch: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(tab) = profile.session.active_query_tab() else {
            return;
        };
        let (id, editor) = (tab.id, tab.editor.clone());
        let Some(select) = tab.last_query.clone() else {
            self.note(
                "Slate does not know which statement produced these rows.".into(),
                cx,
            );
            return;
        };

        let text = editor.read(cx).value().to_string();
        let appended = appended_statement(&text, &batch);
        editor.update(cx, |editor, cx| editor.set_value(appended, window, cx));
        self.execute_and_then(
            batch,
            Tab::Query(id),
            Some(Refresh::Statement(select)),
            false,
            cx,
        );
    }

    /// Run the batch a relation tab is showing. The modal stays up until it
    /// succeeds, so a failure leaves the statement on screen.
    fn run_apply_review(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(review) = &profile.session.apply_review else {
            return;
        };
        let (tab, sql) = (review.tab, review.sql.clone());
        let Tab::Object(id) = tab else {
            return;
        };
        // Without the refresh the grid would show the UPDATE's empty result set
        // and the user would watch their table vanish.
        self.execute_and_then(sql, tab, Some(Refresh::Relation(id)), false, cx);
    }

    /// Put the batch away, leaving the edits pending: reading a statement and
    /// deciding not to run it is not the same as throwing the edits out, which
    /// is what Discard is for.
    fn close_apply_review(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(profile) = self.profile_mut() else {
            return false;
        };
        let closed = profile.session.apply_review.take().is_some();
        if closed {
            cx.notify();
        }
        closed
    }

    /// Back to exactly the rows the server sent.
    fn discard_edits(&mut self, _: &DiscardEdits, _: &mut Window, cx: &mut Context<Self>) {
        let Some(results) = self
            .profile()
            .and_then(|profile| profile.session.active_results().cloned())
        else {
            return;
        };
        results.update(cx, |table, cx| {
            table.delegate_mut().discard_pending();
            table.refresh(cx);
        });
        if let Some(profile) = self.profile_mut() {
            profile.session.apply_review = None;
        }
        cx.notify();
    }

    fn persist_buffer(&self, cx: &App) -> Result<(), String> {
        match self.profile() {
            Some(profile) => {
                write_grids(profile, cx);
                write_buffer(profile, cx)
            }
            None => Ok(()),
        }
    }

    /// Every profile's buffer, for the one moment there is nowhere to report a
    /// failure to: the application is closing.
    fn persist_buffers(&self, cx: &App) {
        for profile in &self.profiles {
            write_grids(profile, cx);
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
            matches!(profile.session.active, Tab::Query(_))
                && profile.session.open_query().is_some()
        })
    }

    fn rename_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self
            .profile()
            .and_then(|profile| profile.session.open_query().map(str::to_string))
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
        let name = profile
            .session
            .save_name
            .read(cx)
            .value()
            .trim()
            .to_string();
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
            Tab::Query(id) => profile
                .session
                .query_tab(id)
                .and_then(|tab| tab.open_query.clone()),
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
            Tab::Query(id) => {
                if let Some(profile) = self.profile_mut() {
                    if let Some(tab) = profile.session.query_tab_mut(id) {
                        tab.open_query = Some(name.clone());
                    }
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

    /// A new empty buffer, beside the ones already open.
    ///
    /// It used to clear the buffer in front, which is why a dirty scratch was
    /// persisted and then emptied: one editor meant a new query had nowhere to
    /// go but on top of the old one.
    fn new_query(&mut self, _: &NewQuery, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(message) = self.persist_buffer(cx) {
            self.note(message, cx);
            return;
        }
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let id = profile.session.next_query_id;
        profile.session.next_query_id += 1;
        profile.session.naming = false;
        profile.session.notice = None;
        let profile_id = profile.id.clone();

        let (tab, _) = QueryTab::restore(
            &profile_id,
            &store::StoredQueryTab {
                id,
                name: None,
                active: true,
            },
            window,
            cx,
        );
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let profile_id = profile.id.clone();
        profile.session.queries.push(tab);
        self.install_completions(&profile_id, cx);
        self.activate_tab(Tab::Query(id), cx);
    }

    /// Close an unsaved buffer. Its scratch file goes with it: an unnamed
    /// buffer is its text, and closing one is discarding both.
    fn close_buffer(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let position = profile.session.queries.iter().position(|tab| tab.id == id);
        let Some(position) = position else {
            return;
        };
        profile.session.queries.remove(position);
        let profile_id = profile.id.clone();

        if profile.session.active == Tab::Query(id) {
            // The neighbour on the left, or the one that slid into this slot.
            let next = profile
                .session
                .queries
                .get(position.saturating_sub(1))
                .map(|tab| tab.id);
            if let Some(next) = next {
                profile.session.active = Tab::Query(next);
                profile.session.editor_needs_focus = true;
            }
        }

        if let Err(message) = store::delete_scratch(&profile_id, id) {
            self.note(message, cx);
        }
        // The tab is gone, so its snapshot has nothing left to come back to --
        // and the ids are reused, so a leftover file would open as another
        // buffer's rows.
        let _ = store::remove_grid(&profile_id, &store::query_grid_key(id));
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Bring a saved query up: its own tab if one is already open, a new one
    /// otherwise. It never lands on top of a buffer someone is writing in.
    fn open_saved_query(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self
            .profile()
            .and_then(|profile| profile.session.tab_holding(&name))
        {
            self.activate_tab(Tab::Query(id), cx);
            return;
        }
        if let Err(message) = self.persist_buffer(cx) {
            self.note(message, cx);
            return;
        }
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let profile_id = profile.id.clone();
        // Read before the tab is built, so a query whose file has gone since
        // the strip was drawn says so instead of opening an empty buffer.
        match store::read_query(&profile_id, &name) {
            Ok(Some(_)) => {}
            Ok(None) => {
                profile.session.saved_queries = store::saved_queries(&profile_id);
                profile.session.notice = Some(format!("{name} no longer exists."));
                cx.notify();
                return;
            }
            Err(message) => {
                profile.session.notice = Some(message);
                cx.notify();
                return;
            }
        }

        let id = profile.session.next_query_id;
        profile.session.next_query_id += 1;
        profile.session.notice = None;

        let (tab, notice) = QueryTab::restore(
            &profile_id,
            &store::StoredQueryTab {
                id,
                name: Some(name),
                active: true,
            },
            window,
            cx,
        );
        let Some(profile) = self.profile_mut() else {
            return;
        };
        profile.session.queries.push(tab);
        profile.session.notice = notice;
        self.install_completions(&profile_id, cx);
        self.activate_tab(Tab::Query(id), cx);
    }

    /// A statement out of the history, back in the buffer.
    ///
    /// Appended rather than swapped in, for the reason `apply_in_buffer`
    /// appends: recalling a statement is not a reason to take away what is
    /// already written, and the statement that runs is the statement on screen.
    /// The cursor lands on it, because that is what `cmd+enter` reads to decide
    /// what to send.
    fn recall_statement(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(tab) = profile.session.active_query_tab() else {
            return;
        };
        let (id, editor) = (tab.id, tab.editor.clone());
        let text = editor.read(cx).value().to_string();
        let appended = appended_statement(&text, &sql);
        let line = appended.lines().count().saturating_sub(sql.lines().count()) as u32;
        editor.update(cx, |editor, cx| {
            editor.set_value(appended, window, cx);
            editor.set_cursor_position(Position::new(line, 0), window, cx);
        });
        self.activate_tab(Tab::Query(id), cx);
    }

    /// The first unsaved buffer, or a new one when every open tab has a name.
    ///
    /// It used to swap the scratch file into the single editor, which is what
    /// made "New Query" a place rather than a tab.
    fn open_scratch_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let unsaved = self.profile().and_then(|profile| {
            profile
                .session
                .queries
                .iter()
                .find(|tab| tab.open_query.is_none())
                .map(|tab| tab.id)
        });
        match unsaved {
            Some(id) => self.activate_tab(Tab::Query(id), cx),
            None => self.new_query(&NewQuery, window, cx),
        }
    }

    /// The chip's own delete: the first click arms it and the second one means
    /// it. Quieter than the dialog `cmd+w` raises, because the trash icon is
    /// already an unambiguous ask and the tab it belongs to is right there.
    fn arm_delete_saved_query(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        if profile.session.pending_delete.as_deref() != Some(&name) {
            profile.session.pending_delete = Some(name);
            cx.notify();
            return;
        }
        self.delete_saved_query(name, cx);
    }

    fn delete_saved_query(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let id = profile.id.clone();
        // Read before the delete, because afterwards nothing on the session
        // still points at the file and only this says which tab did.
        let was_open = profile.session.tab_holding(&name);
        if let Err(message) = store::delete_query(&id, &name) {
            self.note(message, cx);
            return;
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.saved_queries = store::saved_queries(&id);
            profile.session.pending_delete = None;
            profile.session.pending_close = None;
            profile.session.notice = Some(format!("Deleted {name}."));

            // The tab goes with the file. Its text was the query, and the query
            // is what was deleted -- keeping it in an untitled buffer would
            // leave `cmd+w` looking like it had done nothing.
            if let Some(open) = was_open {
                if profile.session.queries.len() > 1 {
                    profile.session.queries.retain(|tab| tab.id != open);
                    // With the tab, as in `close_buffer`: a snapshot with no
                    // tab left to come back to is rows the next buffer to be
                    // handed this id would show as its own.
                    let _ = store::remove_grid(&id, &store::query_grid_key(open));
                    if profile.session.active == Tab::Query(open)
                        && let Some(next) = profile.session.queries.first().map(|tab| tab.id)
                    {
                        profile.session.active = Tab::Query(next);
                        profile.session.editor_needs_focus = true;
                    }
                } else if let Some(tab) = profile.session.query_tab_mut(open) {
                    // The only buffer. A profile always has somewhere to write,
                    // so it is unnamed from here rather than closed, and the
                    // strip keeps a place to type in.
                    tab.open_query = None;
                }
            }
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
        self.execute_and_then(sql, tab, None, false, cx);
    }

    /// As `execute_sql`, with something to run once this statement has
    /// succeeded.
    ///
    /// `keep_rows` leaves whatever the grid is showing in place until the new
    /// result lands, for the refresh of a tab whose rows came off disk. Every
    /// other run clears them first, because rows from the previous statement
    /// sitting under the one now running cannot be told from fresh ones.
    ///
    /// Chained inside the completion rather than called after it: `execute_sql`
    /// refuses to start while a query is running, so a second call made here
    /// would be dropped on the floor. Nothing follows a failure — the error is
    /// what there is to see, and a refresh would replace it with rows.
    fn execute_and_then(
        &mut self,
        sql: String,
        tab: Tab,
        refresh: Option<Refresh>,
        keep_rows: bool,
        cx: &mut Context<Self>,
    ) {
        // Read before the task, which outlives the borrow of `self`.
        let engine = self.engine();
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
        if !keep_rows {
            results.update(cx, |table, cx| {
                *table.delegate_mut() = ResultGrid::empty();
                // The inspector reads whatever row is selected, and a row index
                // means nothing once the rows behind it are gone.
                table.clear_selection(cx);
                table.refresh(cx);
            });
        }
        cx.notify();

        // Read from the statement that is about to run, so the headers say what
        // the rows on screen are actually ordered by rather than what Slate
        // last intended to ask for.
        let keys = sql::order_by(&sql);
        let sortable = keys.is_some();
        let keys = keys.unwrap_or_default();
        // Kept only where it is read back: the query tab's grid has to be able
        // to say which statement produced it.
        let statement = matches!(tab, Tab::Query(_)).then(|| sql.clone());
        // Recorded on the way out rather than on the way back: the history is
        // what the user ran, and a statement that failed is exactly the one
        // worth getting back. Only the buffer's — a relation's preview is SQL
        // Slate wrote, and nobody asked to keep it.
        if let Some(statement) = &statement
            && let Some(profile) = self.profile_mut()
        {
            // A line that could not be written is not worth a notice on every
            // run: the statement is still in the buffer, so nothing is lost.
            let _ = store::append_history(&profile.id, statement);
            remember_statement(&mut profile.session.history, statement);
        }
        let query_task = cx
            .background_executor()
            .spawn(async move { connection.query(&sql) });

        cx.spawn(async move |workspace, cx| {
            let result = query_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    let (succeeded, produced_grid) = {
                        let Some(profile) = workspace.issued_to(&id, generation) else {
                            workspace.drop_stale_run(&id, tab, cx);
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
                                let produced_grid = !result.columns.is_empty();
                                results.update(cx, |table, cx| {
                                    let sort = sort_columns(engine, &keys, &result.columns);
                                    *table.delegate_mut() =
                                        ResultGrid::new(result).with_sort(sort, sortable);
                                    table.refresh(cx);
                                });
                                (true, produced_grid)
                            }
                            Err(error) => {
                                *state = QueryState::Failed(error);
                                (false, false)
                            }
                        }
                    };

                    if succeeded && let Some(profile) = workspace.issued_to(&id, generation) {
                        // A statement that returned no columns produced no grid,
                        // so it is not the statement to go back to — which is
                        // what keeps an applied UPDATE from becoming the query
                        // an apply re-runs.
                        if produced_grid
                            && let Some(statement) = statement
                            && let Tab::Query(query) = tab
                            && let Some(tab) = profile.session.query_tab_mut(query)
                        {
                            tab.last_query = Some(statement);
                        }
                        // Nothing left to read once the batch it was showing has
                        // run.
                        if refresh.is_some() {
                            profile.session.apply_review = None;
                        }
                    }
                    cx.notify();

                    if succeeded && let Some(refresh) = refresh {
                        match refresh {
                            Refresh::Statement(sql) => workspace.execute_sql(sql, tab, cx),
                            Refresh::Relation(id) => workspace.refresh_relation(id, cx),
                        }
                    }
                })
                .ok();
        })
        .detach();
    }

    /// A result dropped because its generation was retired -- a reconnect or a
    /// profile switch bumped it while the statement was in flight.
    ///
    /// The tab it was for is still `Running`, showing a spinner over rows
    /// nothing is going to replace, and `load_relation` will not re-query a tab
    /// that is not `Idle` or `Failed`. `Idle` rather than `Failed`, because
    /// nothing failed: the run was abandoned, and `Idle` is what makes the next
    /// visit to the tab run it again.
    fn drop_stale_run(&mut self, id: &str, tab: Tab, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) else {
            return;
        };
        let Some((state, _)) = profile.session.slot(tab) else {
            return;
        };
        if matches!(state, QueryState::Running) {
            *state = QueryState::Idle;
            cx.notify();
        }
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
        let form = self
            .form
            .as_ref()
            .expect("form is rendered only while open");
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
                                            .child("Connect to a database"),
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
                                    .child("Engine"),
                            )
                            .child(
                                div().flex().gap(px(layout::SPACE_XS)).children(
                                    Engine::ALL.map(|engine| self.engine_chip(engine, cx)),
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
                                        icon_button(
                                            "apply-connection-url",
                                            icon::FILL_DOWN,
                                            Tone::Primary,
                                            Control::Standard,
                                            t,
                                        )
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
                    // An engine that is a file has no host, no credentials and
                    // no transport, so those fields are absent rather than
                    // present and inert. A disabled field still reads as
                    // something the connection has.
                    .children(
                        (!form.engine.is_server())
                            .then(|| self.form_field("Database file", &form.path, cx)),
                    )
                    .children(form.engine.is_server().then(|| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(layout::SPACE_MD))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(layout::SPACE_SM))
                                    .child(
                                        div()
                                            .flex_1()
                                            .child(self.form_field("Host", &form.host, cx)),
                                    )
                                    .child(
                                        div()
                                            .w(px(96.))
                                            .child(self.form_field("Port", &form.port, cx)),
                                    ),
                            )
                            .child(self.form_field("Database", &form.database, cx))
                            .child(self.form_field("Username", &form.user, cx))
                            .child(self.form_field("Password", &form.password, cx))
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
                                            .child("Encryption"),
                                    )
                                    .child(div().flex().gap(px(layout::SPACE_XS)).children(
                                        SslMode::ALL.map(|mode| self.sslmode_chip(mode, cx)),
                                    ))
                                    // Five words do not say which ones check who
                                    // answered, and that is the whole difference
                                    // between them.
                                    .child(
                                        div()
                                            .text_size(px(layout::TEXT_XS))
                                            .text_color(t.text_faint)
                                            .child(form.sslmode.explanation()),
                                    ),
                            )
                            // Only where it is consulted: on `require` a
                            // certificate file changes nothing, and a field that
                            // changes nothing reads as though it does.
                            .children(form.sslmode.checks_certificate().then(|| {
                                self.form_field("Root certificate", &form.root_certificate, cx)
                            }))
                    }))
                    // Outside the server block: every engine can stop a
                    // statement, and this is the only place a profile has to
                    // say for how long -- there is no settings window and is
                    // not going to be one.
                    .child(self.form_field("Statement timeout", &form.statement_timeout, cx))
                    .children(message.map(|message| {
                        div()
                            .text_size(px(layout::TEXT_SM))
                            .text_color(t.danger)
                            .child(message)
                    }))
                    .child(
                        div()
                            .flex()
                            .gap(px(layout::SPACE_SM))
                            // `escape` is the other way out, and on the first
                            // launch there is nowhere to back out to: the form
                            // is the whole application until a profile exists.
                            .children((!self.profiles.is_empty()).then(|| {
                                button("cancel", "Cancel", Tone::Quiet, Control::Standard, t)
                                    .flex_1()
                                    .on_click(cx.listener(|workspace, _, window, cx| {
                                        workspace.show_editor(&ShowEditor, window, cx);
                                    }))
                            }))
                            .child(
                                button("connect", "Connect", Tone::Primary, Control::Standard, t)
                                    .flex_1()
                                    .on_click(cx.listener(Self::connect)),
                            ),
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

    /// The palette, centred over everything else.
    ///
    /// `key_context` is load-bearing: the arrow keys are bound against
    /// `Palette > Input`, which is the only predicate deep enough to win the
    /// keystroke back from the search field. See `move_palette_selection`.
    fn render_palette(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = *theme(cx);
        let list = self.palette.as_ref()?;
        let placeholder = list.read(cx).delegate().placeholder();
        let workspace = cx.entity().downgrade();

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                // Cross-axis stretch is the flex default, and it would take the
                // palette's own height with it: a box down to the bottom of the
                // window, with the list capped at its own max height near the
                // top and the rest of the panel painted empty.
                .items_start()
                .child(
                    div()
                        .id("palette")
                        .key_context("Palette")
                        // Below the titlebar rather than centred vertically:
                        // the eye is already at the top of the window, and the
                        // list grows downwards from a fixed line.
                        .mt(px(layout::TITLEBAR_HEIGHT * 2.))
                        .w(px(layout::PALETTE_WIDTH))
                        .bg(t.overlay)
                        .border_1()
                        .border_color(t.border_strong)
                        .rounded(px(layout::RADIUS_PANEL))
                        .shadow_lg()
                        .overflow_hidden()
                        .child(
                            List::new(list)
                                .search_placeholder(placeholder)
                                .max_h(px(layout::PALETTE_MAX_HEIGHT)),
                        )
                        .on_mouse_down_out(move |_, _, cx| {
                            _ = workspace.update(cx, |workspace, cx| {
                                workspace.close_palette(cx);
                            });
                        }),
                )
                .into_any_element(),
        )
    }

    /// What `cmd+w` asks before it takes unapplied cell edits with the tab.
    fn render_discard_confirmation(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = *theme(cx);
        self.profile()?.session.pending_discard.as_ref()?;
        let cancel_workspace = cx.entity().downgrade();
        let discard_workspace = cancel_workspace.clone();

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    dialog(t)
                        .child(section_label(t, "Close tab"))
                        .child(
                            div()
                                .text_size(px(layout::TEXT_SM))
                                .text_color(t.text_muted)
                                // The edits are held against the fetched rows
                                // and never written to them, so closing the
                                // tab is the moment they stop existing.
                                .child(
                                    "This tab has cell edits that have not been applied. \
                                     Closing it discards them.",
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    button(
                                        "cancel-discard-close",
                                        "Cancel",
                                        Tone::Quiet,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            _ = cancel_workspace.update(cx, |workspace, cx| {
                                                workspace.cancel_discard_close(cx);
                                            });
                                        },
                                    ),
                                )
                                .child(
                                    button(
                                        "confirm-discard-close",
                                        "Discard",
                                        Tone::Danger,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            _ = discard_workspace.update(cx, |workspace, cx| {
                                                workspace.confirm_discard_close(cx);
                                            });
                                        },
                                    ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// What `cmd+w` asks before it takes a saved query with the tab.
    fn render_close_confirmation(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = *theme(cx);
        let name = self.profile()?.session.pending_close.clone()?;
        let cancel_workspace = cx.entity().downgrade();
        let delete_workspace = cancel_workspace.clone();
        let deleted = name.clone();

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    dialog(t)
                        .child(section_label(t, "Close query"))
                        .child(
                            div()
                                .text_size(px(layout::TEXT_SM))
                                .text_color(t.text_muted)
                                // The whole point of the dialog: a saved query
                                // is listed while its file exists, so closing
                                // its tab and deleting it are one act.
                                .child(format!(
                                    "{name} is a saved query. Closing its tab deletes it."
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    button(
                                        "cancel-close-tab",
                                        "Cancel",
                                        Tone::Quiet,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            _ = cancel_workspace.update(cx, |workspace, cx| {
                                                workspace.cancel_close_tab(cx);
                                            });
                                        },
                                    ),
                                )
                                .child(
                                    button(
                                        "confirm-close-tab",
                                        "Delete",
                                        Tone::Danger,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            _ = delete_workspace.update(cx, |workspace, cx| {
                                                workspace.delete_saved_query(deleted.clone(), cx);
                                            });
                                        },
                                    ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// A relation tab's generated batch, on screen before it runs.
    ///
    /// The statement is the point of the panel: a relation tab has no buffer, so
    /// this is where rule 1's "the statement that runs is the statement on
    /// screen" is satisfied, and Run is the ask.
    ///
    /// It stays up until the batch succeeds. `execute_sql` clears the grid as it
    /// starts and the pending edits go with it, so after a failure this is the
    /// only remaining copy of what was attempted.
    fn render_apply_review(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = *theme(cx);
        let code = fonts(cx).editor.clone();
        let profile = self.profile()?;
        let review = profile.session.apply_review.as_ref()?;
        if review.tab != profile.session.active {
            return None;
        }
        // The batch is the only thing this tab can have run while the panel is
        // open, so a failure on it is this batch's failure.
        let error = match profile.session.active_query() {
            Some(QueryState::Failed(error)) => Some(error.message.clone()),
            _ => None,
        };
        let running = matches!(profile.session.active_query(), Some(QueryState::Running));
        let lines: Vec<String> = review.sql.lines().map(str::to_string).collect();
        let cancel_workspace = cx.entity().downgrade();
        let run_workspace = cancel_workspace.clone();

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    dialog(t)
                        .child(section_label(t, "Apply edits"))
                        .child(
                            div()
                                .id("apply-review-sql")
                                .max_h(px(220.))
                                .overflow_y_scroll()
                                .font_family(code)
                                .text_size(px(layout::TEXT_SM))
                                // Line by line: a single child carrying newlines
                                // is one run of text to the layout.
                                .children(lines.into_iter().map(|line| div().child(line))),
                        )
                        .children(error.map(|message| {
                            div()
                                .text_size(px(layout::TEXT_SM))
                                .text_color(t.danger)
                                .child(message)
                        }))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    button(
                                        "cancel-apply",
                                        "Cancel",
                                        Tone::Quiet,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            _ = cancel_workspace.update(cx, |workspace, cx| {
                                                workspace.close_apply_review(cx);
                                            });
                                        },
                                    ),
                                )
                                .child(
                                    // Quiet while it runs, because the library's
                                    // disabled fill is the only thing that
                                    // dims and our pinned label would stay
                                    // bright over it.
                                    button(
                                        "run-apply",
                                        "Run",
                                        if running { Tone::Quiet } else { Tone::Primary },
                                        Control::Standard,
                                        t,
                                    )
                                    .disabled(running)
                                    .on_click(
                                        move |_, _, cx| {
                                            _ = run_workspace.update(cx, |workspace, cx| {
                                                workspace.run_apply_review(cx);
                                            });
                                        },
                                    ),
                                ),
                        ),
                )
                .into_any_element(),
        )
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
                                    remove
                                        .opacity(0.)
                                        .group_hover(format!("profile-row-{index}"), |style| {
                                            style.opacity(1.)
                                        })
                                })
                                .child(
                                    // Armed, it says the word and takes the
                                    // danger fill: the icon alone asks, the
                                    // red confirms.
                                    icon_button(
                                        ("remove-profile", index),
                                        icon::DELETE,
                                        if pending { Tone::Danger } else { Tone::Quiet },
                                        Control::Inline,
                                        t,
                                    )
                                    .when(pending, |armed| {
                                        armed.w_auto().px(px(layout::SPACE_XS)).child(button_label(
                                            "Remove?",
                                            Tone::Danger,
                                            Control::Inline,
                                            t,
                                        ))
                                    })
                                    .tooltip("Remove connection")
                                    .on_click(
                                        move |_, window, cx| {
                                            _ = remove_workspace.update(cx, |workspace, cx| {
                                                workspace.remove_profile(index, window, cx);
                                            });
                                        },
                                    ),
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
                .child(div().my(px(layout::SPACE_XS)).h(px(1.)).bg(t.border))
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
                            // `ListItem` sizes its text in `rems`, which tracks
                            // the library's 16 rather than Slate's body size.
                            .text_size(px(layout::TEXT_MD))
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
                        // Every opened object gets a tab that stays until it is
                        // closed, so a second click on the same row is the same
                        // gesture as the first.
                        row.on_click(move |_, window, cx| {
                            _ = workspace.update(cx, |workspace, cx| {
                                workspace.open_explorer_target(leaf.target, window, cx);
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
        // Where a query tab that reached the front without `activate_tab` gets
        // its snapshot read. `session.active` is written in six places --
        // `Session::new`, `activate_tab`, `close_object`, `escape`,
        // `close_buffer` and `delete_saved_query` -- and all but the first two
        // set a `Tab::Query` without hydrating it, so this cannot be narrowed
        // to the opening tab.
        //
        // What keeps it off a live result is `hydrate_tab`'s own `Idle` guard,
        // which holds only because no reachable state leaves a query tab
        // `Idle` while its grid holds rows: a run blanks the grid before it
        // starts, and nothing sets `Idle` back afterwards. A "clear results"
        // or a cancel that resets state would break that, and this line would
        // then read a snapshot over rows the user is looking at.
        //
        // Gated on a flag rather than on the disk, so every later frame is a
        // bool test.
        if let Some(active @ Tab::Query(_)) = self.profile().map(|profile| profile.session.active) {
            self.hydrate_tab(active, cx);
        }

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
                Tab::Query(id) => Focus::Buffer(profile.session.query_tab(id)?.editor.clone()),
                Tab::Object(id) => {
                    let tab = profile.session.objects.iter().find(|tab| tab.id == id)?;
                    match &tab.body {
                        ObjectBody::Relation { results, .. } => Focus::Grid(results.clone()),
                        // A routine's tab is read: nothing in it takes a
                        // keystroke. The window still has to hold focus, or
                        // the bindings that leave this tab go with it.
                        ObjectBody::Routine(_) => Focus::Window,
                    }
                }
            };
            profile.session.editor_needs_focus = false;
            Some(focus)
        });
        // Before the tab's own focus, and separately: the form is a surface of
        // its own, and a field it just unmounted took the window's only
        // dispatch path with it.
        if let Some(input) = self.form.as_mut().and_then(|form| form.needs_focus.take()) {
            input.focus_handle(cx).focus(window);
        }

        match take_focus {
            Some(Focus::Buffer(input)) => input.focus_handle(cx).focus(window),
            Some(Focus::Grid(grid)) => grid.focus_handle(cx).focus(window),
            Some(Focus::Window) => self.focus.focus(window),
            None => {}
        }

        // Last, and unconditionally: the palette is modal, and it holds the
        // keyboard against anything above that just claimed it. One a modal
        // cannot be typed into is one that cannot be dismissed either.
        if let Some(list) = &self.palette {
            let handle = list.focus_handle(cx);
            if !handle.is_focused(window) {
                handle.focus(window);
            }
        }

        if self.form.is_some() {
            return div()
                .id("connection-form")
                .size_full()
                // The floor under the focus, the same one the workspace root
                // has: without it the form's bindings dispatch nowhere the
                // moment no field holds focus.
                .track_focus(&self.focus)
                // Chrome, so the form's card is the raised plane on it.
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
                .child(titlebar(t, None, None))
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

        // The rows the grid actually holds, which is fewer than the result had
        // whenever a restored snapshot was capped.
        let showing = profile
            .session
            .active_results()
            .map(|results| results.read(cx).delegate().result().rows.len());
        let snapshot_age = profile
            .session
            .active_results()
            .and_then(|results| results.read(cx).delegate().captured())
            .map(|captured| relative_age(store::captured_at().saturating_sub(captured)));
        let query_status = match profile.session.active_query() {
            Some(QueryState::Complete {
                rows,
                bytes,
                elapsed,
                ..
            }) => {
                let count = row_readout(showing.unwrap_or(*rows), *rows);
                // A snapshot knows neither how many bytes crossed the wire nor
                // how long it took, so reporting `0 B · 0.0ns` invents two
                // numbers. What it does know is when it was taken.
                Some(match snapshot_age {
                    Some(age) => format!("{count} · snapshot from {age} ago"),
                    None => format!("{count} · {} · {elapsed:.1?}", human_bytes(*bytes as u64)),
                })
            }
            _ => None,
        };
        let notice = profile.session.notice.clone();
        let has_pending = self.has_pending_edits(cx);
        let has_results = self.has_results(cx);
        let apply_workspace = cx.entity().downgrade();
        let discard_workspace = apply_workspace.clone();
        let csv_workspace = apply_workspace.clone();
        let json_workspace = apply_workspace.clone();

        let content = div()
            // Flush, not a floating card: the split handle already draws the
            // one seam, and the planes inside separate by tone.
            .size_full()
            .min_w_0()
            .child(views::render_main_content(profile, cx));
        // With the sidebar folded there is nothing to split, and a split with
        // one panel still paints the handle it no longer divides anything with.
        let main_pane = if self.sidebar_hidden {
            content.into_any_element()
        } else {
            h_resizable("workspace-shell-split")
                .child(
                    resizable_panel()
                        .size(px(layout::SIDEBAR_DEFAULT_WIDTH))
                        .size_range(px(layout::SIDEBAR_MIN_WIDTH)..px(layout::SIDEBAR_MAX_WIDTH))
                        .child(self.render_explorer(profile, cx)),
                )
                .child(resizable_panel().child(content))
                .into_any_element()
        };

        div()
            .id("workspace")
            .relative()
            // The floor under the focus, so a surface with nothing focusable
            // on it still has a dispatch path for the workspace's own
            // bindings. An inner element that can take focus claims it first
            // and stops this one from taking it back.
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::run_query))
            .on_action(cx.listener(Self::cancel_query))
            .on_action(cx.listener(Self::apply_edits))
            .on_action(cx.listener(Self::discard_edits))
            .on_action(cx.listener(Self::sort_column))
            .on_action(cx.listener(Self::set_row_limit))
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
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::fuzzy_open))
            .on_action(cx.listener(Self::command_palette))
            .on_action(cx.listener(Self::palette_next))
            .on_action(cx.listener(Self::palette_previous))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::accept_completion))
            .size_full()
            // The shell is the frost: titlebar, sidebar and status bar paint
            // nothing of their own, they are the glass the window root already
            // laid down. The editor and the results step forward from it by
            // tone, and by letting less of the desktop through.
            .text_color(t.text)
            .text_size(px(layout::TEXT_MD))
            .flex()
            .flex_col()
            .child(titlebar(
                t,
                Some(profile.name.clone()),
                Some(
                    icon_button(
                        "toggle-sidebar",
                        icon::SIDEBAR,
                        Tone::Quiet,
                        Control::Compact,
                        t,
                    )
                    .on_click(cx.listener(|workspace, _, window, cx| {
                        workspace.toggle_sidebar(&ToggleSidebar, window, cx);
                    }))
                    .into_any_element(),
                ),
            ))
            .child(div().flex_1().min_h_0().child(main_pane))
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
                    .children(notice.map(|notice| div().text_color(t.text_muted).child(notice)))
                    // One right-hand cluster, so there is a single `ml_auto`
                    // in the row: two of them split the free space between
                    // them and strand the readout in the middle of the bar.
                    //
                    // Each control appears only when it does something. A pair
                    // of buttons that do nothing is a pair to read past, and
                    // Apply has no keybinding on purpose -- see `apply_edits`.
                    .child(
                        div()
                            .ml_auto()
                            .flex()
                            .items_center()
                            .gap(px(layout::SPACE_SM))
                            .children(query_status.map(|query_status| {
                                div().text_color(t.text_faint).child(query_status)
                            }))
                            // Named, not one button over a menu: the choice is
                            // between two things, and a control that opens
                            // another control to ask which is a click spent on
                            // nothing. It also puts the format on screen, which
                            // a lone "Export" left to the file extension.
                            .children(has_results.then(|| {
                                button("export-csv", "Export CSV", Tone::Quiet, Control::Compact, t)
                                    .on_click(move |_, _, cx| {
                                        _ = csv_workspace.update(cx, |workspace, cx| {
                                            workspace.export_results(Format::Csv, cx);
                                        });
                                    })
                            }))
                            .children(has_results.then(|| {
                                button(
                                    "export-json",
                                    "Export JSON",
                                    Tone::Quiet,
                                    Control::Compact,
                                    t,
                                )
                                .on_click(move |_, _, cx| {
                                    _ = json_workspace.update(cx, |workspace, cx| {
                                        workspace.export_results(Format::Json, cx);
                                    });
                                })
                            }))
                            .children(has_pending.then(|| {
                                button("discard-edits", "Discard", Tone::Quiet, Control::Compact, t)
                                    .on_click(move |_, window, cx| {
                                        _ = discard_workspace.update(cx, |workspace, cx| {
                                            workspace.discard_edits(&DiscardEdits, window, cx);
                                        });
                                    })
                            }))
                            .children(has_pending.then(|| {
                                button(
                                    "apply-edits",
                                    "Apply edits",
                                    Tone::Primary,
                                    Control::Compact,
                                    t,
                                )
                                .on_click(move |_, window, cx| {
                                    _ = apply_workspace.update(cx, |workspace, cx| {
                                        workspace.apply_edits(&ApplyEdits, window, cx);
                                    });
                                })
                            })),
                    ),
            )
            .children(self.render_apply_review(cx))
            .children(self.render_close_confirmation(cx))
            .children(self.render_discard_confirmation(cx))
            .children(self.render_palette(cx))
    }
}

/// The icon a sidebar row carries: a grid for a table, layers for one split
/// into partitions, an eye for the kinds that are a saved query over a table, a
/// disk for the one that stores its answer, and a globe for the one that lives
/// on another server entirely.
/// Slate's statement for a relation's tab, carrying the sort the headers asked
/// for. Regenerated rather than edited, so the row limit and the quoting stay
/// in one place.
fn relation_sql(
    engine: Engine,
    schema: &str,
    relation: &str,
    sort: &[SortKey],
    limit: usize,
) -> String {
    let preview = preview_sql(engine, schema, relation, limit);
    sql::with_order_by(&preview, sort).unwrap_or(preview)
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
    let statements: Option<Vec<String>> = rows
        .iter()
        .map(|row| {
            sql::update_row(
                engine,
                &row.schema,
                &row.table,
                &borrowed(&row.sets),
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

/// How a column is named in an `ORDER BY`.
///
/// By name, so the statement reads as something a person would have written --
/// except where a name cannot identify one column, and then by position, which
/// always can. Duplicate names come back from any join written with `*`.
fn sort_expression(engine: Engine, columns: &[db::Column], column: usize) -> Option<String> {
    let name = &columns.get(column)?.name;
    let unique = columns.iter().filter(|other| &other.name == name).count() == 1;

    Some(match unique && !name.is_empty() {
        true => engine.quote_identifier(name),
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
fn sort_columns(engine: Engine, keys: &[SortKey], columns: &[db::Column]) -> Vec<(usize, bool)> {
    keys.iter()
        .filter_map(|key| {
            let expression = key.expression.trim();
            let named = engine.unquote_identifier(expression);
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
            &store::object_grid_key(&tab.schema, &tab.name),
            &store::StoredGrid {
                limit: Some(*limit),
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

/// One column of icons down the sidebar, so every label starts at the same x
/// whether its row is a folder or an object.
fn row_icon(t: Theme, path: &'static str) -> impl IntoElement {
    icon(path)
        .size(px(layout::ICON_SIZE))
        .text_color(t.text_faint)
}

/// Slate's own titlebar, drawn where the platform's would be.
///
/// The system titlebar is transparent (see `main`), so this row is what runs to
/// the top of the window and the window buttons are drawn over its leading
/// inset. It is also the drag handle the platform no longer provides — which
/// is why the drag region is a child covering what is left of the row rather
/// than the row itself: a drag region swallows the clicks a button needs, so
/// anything interactive goes in `leading`, outside it.
fn titlebar(t: Theme, subtitle: Option<String>, leading: Option<AnyElement>) -> impl IntoElement {
    div()
        .h(px(layout::TITLEBAR_HEIGHT))
        .w_full()
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(layout::SPACE_MD))
        .pl(px(layout::TITLEBAR_LEADING_INSET))
        .pr(px(layout::SPACE_MD))
        .children(leading)
        .child(
            div()
                .id("titlebar")
                .window_control_area(gpui::WindowControlArea::Drag)
                .on_double_click(|_, window, _| window.titlebar_double_click())
                .flex_1()
                .h_full()
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
}

/// The quietest thing on screen: small, uppercase, and dim enough that the
/// names under it are what the eye lands on first.
/// The card every modal is drawn on. Shared so two panels asking the same kind
/// of question cannot end up looking like two different applications.
fn dialog(t: Theme) -> gpui::Div {
    div()
        .w(px(layout::DIALOG_WIDTH))
        .p(px(layout::SPACE_LG))
        .flex()
        .flex_col()
        .gap(px(layout::SPACE_MD))
        .bg(t.overlay)
        .border_1()
        .border_color(t.border_strong)
        .rounded(px(layout::RADIUS_PANEL))
        .shadow_lg()
}

/// What a button's fill says. Colour is state here as everywhere else: a Slate
/// button is the neutral control tone unless it is the one action its surface
/// exists to take, or the one that destroys something.
#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Primary,
    Quiet,
    Danger,
}

/// Standard for a dialog or the connection form, where a button sits beside a
/// field and is the thing the surface exists to click. Compact for the strips
/// that are themselves only a control tall. Inline for the affordance revealed
/// on a chip or a row it does not own.
#[derive(Clone, Copy, PartialEq)]
enum Control {
    Standard,
    Compact,
    Inline,
}

impl Tone {
    /// The colour of the label and the icon, pinned rather than inherited — see
    /// [`button`].
    fn ink(self, t: Theme) -> theme::color::Srgb {
        match self {
            Tone::Primary => t.text,
            Tone::Quiet => t.text_muted,
            Tone::Danger => t.on_accent,
        }
    }
}

impl Control {
    fn height(self) -> f32 {
        match self {
            Control::Standard => layout::CONTROL_HEIGHT,
            Control::Compact => layout::CONTROL_HEIGHT_COMPACT,
            Control::Inline => layout::CONTROL_HEIGHT_INLINE,
        }
    }

    fn text_size(self) -> f32 {
        match self {
            Control::Standard => layout::TEXT_MD,
            Control::Compact | Control::Inline => layout::TEXT_SM,
        }
    }
}

/// A Slate button.
///
/// gpui-component supplies the mechanism — the tooltip and the keybinding in
/// it, the focus ring, the disabled gate — and none of the appearance survives
/// contact with it. Its size scale bottoms out at a 20px box with 4px of
/// padding, its label comes out at the library's 16px body rather than Slate's
/// 13, and 0.5.1 tints button content `red_400` on hover from a hardcoded
/// colour no theme token reaches.
///
/// So the box is measured here off the layout scale, and the content goes in as
/// a child carrying its own colour. That last part is what settles the hover
/// tint: a child that sets a colour does not inherit the container's.
fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<gpui::SharedString>,
    tone: Tone,
    size: Control,
    t: Theme,
) -> Button {
    control(id, tone, size)
        .px(px(layout::SPACE_MD))
        .when(size == Control::Standard, |standard| {
            standard.min_w(px(layout::CONTROL_MIN_WIDTH))
        })
        .child(button_label(label, tone, size, t))
}

/// A button that is only its icon, so it is square and reads as an
/// affordance beside the thing it acts on rather than as a control of its own.
fn icon_button(
    id: impl Into<gpui::ElementId>,
    path: &'static str,
    tone: Tone,
    size: Control,
    t: Theme,
) -> Button {
    control(id, tone, size).w(px(size.height())).p_0().child(
        icon(path)
            .size(px(layout::ICON_SIZE))
            .text_color(tone.ink(t)),
    )
}

/// A button's words. Separate so the two delete buttons, which grow a
/// confirmation beside their icon once armed, can add them without becoming a
/// different control.
fn button_label(
    label: impl Into<gpui::SharedString>,
    tone: Tone,
    size: Control,
    t: Theme,
) -> impl IntoElement {
    div()
        .flex_none()
        // Or the descenders decide where the text sits in the box.
        .line_height(gpui::relative(1.))
        .text_size(px(size.text_size()))
        .font_weight(FontWeight::MEDIUM)
        .text_color(tone.ink(t))
        .child(label.into())
}

/// The box, without its content. Radius comes from the theme, which is already
/// pointed at `RADIUS_CONTROL`.
fn control(id: impl Into<gpui::ElementId>, tone: Tone, size: Control) -> Button {
    Button::new(id)
        .map(|button| match tone {
            Tone::Primary => button.primary(),
            Tone::Quiet => button.ghost(),
            Tone::Danger => button.danger(),
        })
        .h(px(size.height()))
}

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
/// A row count as a chip label: `1K` rather than `1,000`, because four of these
/// sit side by side and the grouped form is twice as wide for no more meaning.
fn compact_count(rows: usize) -> String {
    match rows >= 1_000 && rows.is_multiple_of(1_000) {
        true => format!("{}K", rows / 1_000),
        false => rows.to_string(),
    }
}

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

/// The row count for the status bar.
///
/// A restored snapshot keeps at most [`store::GRID_ROW_CAP`] rows of what the
/// result had, so the grid can hold fewer rows than the count it reports --
/// and "20,000 rows" over 5,000 of them is a number nobody can act on. Every
/// other result reads as the plain count it always did.
fn row_readout(showing: usize, total: usize) -> String {
    let unit = if total == 1 { "row" } else { "rows" };
    if showing < total {
        return format!(
            "{} of {} {unit}",
            group_thousands(showing as u64),
            group_thousands(total as u64)
        );
    }
    format!("{} {unit}", group_thousands(total as u64))
}

/// How long ago, at the one unit worth reading in a status bar. A cache's age
/// is read to decide whether to trust it, and no such decision turns on the
/// difference between 121 and 122 minutes.
fn relative_age(seconds: u64) -> String {
    match seconds {
        ..60 => "moments".to_string(),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
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
            KeyBinding::new("ctrl-tab", NextProfile, None),
            KeyBinding::new("ctrl-shift-tab", PreviousProfile, None),
            KeyBinding::new("escape", ShowEditor, None),
            KeyBinding::new("cmd-shift-t", CycleTheme, None),
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
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.set_menus(vec![Menu {
            name: "Slate".into(),
            items: vec![MenuItem::action("Quit Slate", Quit)],
        }]);

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
    fn only_the_tab_that_is_a_file_is_asked_about_before_it_closes() {
        let saved = |name: &str| Some(CloseTarget::SavedQuery(name.to_string()));

        assert_eq!(
            close_target(Tab::Object(3), None, 1),
            Some(CloseTarget::Object(3))
        );
        assert_eq!(
            close_target(Tab::Query(0), Some("daily"), 1),
            saved("daily")
        );
        // A profile always has somewhere to write, so the last unsaved buffer
        // has nothing for `cmd+w` to close and nothing to ask about.
        assert_eq!(close_target(Tab::Query(0), None, 1), None);
        // One of several, though, is a scratch pad someone is done with: it
        // goes without a question, the way an unnamed buffer does everywhere.
        assert_eq!(
            close_target(Tab::Query(7), None, 2),
            Some(CloseTarget::Buffer(7))
        );
        // What the query tab happens to be holding says nothing about an
        // object tab, which is the one in front.
        assert_eq!(
            close_target(Tab::Object(3), Some("daily"), 2),
            Some(CloseTarget::Object(3))
        );
    }

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
        assert_eq!(
            sort_expression(Engine::Postgres, &unique, 1),
            Some(r#""name""#.into())
        );

        // `SELECT *` across a join returns the same name twice, and ordering by
        // it would be ambiguous -- so the position, which never is.
        let duplicated = columns(&["id", "id"]);
        assert_eq!(
            sort_expression(Engine::Postgres, &duplicated, 1),
            Some("2".into())
        );
        assert_eq!(sort_expression(Engine::Postgres, &unique, 7), None);

        // MySQL reads a double-quoted name as a *string literal*, so ordering
        // by one is ordering by a constant: every row compares equal, the
        // server raises nothing, and the grid comes back in the same order it
        // went out. This is the assertion that catches that.
        assert_eq!(
            sort_expression(Engine::MySql, &unique, 1),
            Some("`name`".into())
        );
        assert_eq!(
            sort_expression(Engine::Sqlite, &unique, 1),
            Some(r#""name""#.into())
        );

        // A quote in a column name would otherwise end the identifier early.
        let odd = columns(&["we\"ird"]);
        assert_eq!(
            sort_expression(Engine::Postgres, &odd, 0),
            Some("\"we\"\"ird\"".into())
        );
    }

    #[test]
    fn a_sort_key_finds_its_way_back_to_the_header_it_came_from() {
        // The round trip every engine has to survive: the expression written
        // into the statement is the one read back out to light the header up,
        // and the quoting in between is the engine's own.
        let result = columns(&["id", "name"]);
        for engine in Engine::ALL {
            let expression = sort_expression(engine, &result, 1).expect("a unique name");
            assert_eq!(
                sort_columns(engine, &[SortKey::new(expression, false)], &result),
                vec![(1, false)],
                "{engine:?}"
            );
        }
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

        assert_eq!(
            sort_columns(Engine::Postgres, &keys, &result),
            vec![(1, false), (0, true)]
        );
    }

    #[test]
    fn a_preview_asks_for_the_rows_its_tab_was_set_to() {
        assert_eq!(
            relation_sql(Engine::Postgres, "public", "accounts", &[], 100),
            r#"SELECT * FROM "public"."accounts" LIMIT 100"#
        );
        // A raised limit still keeps the sort ahead of it, or the rows would be
        // ordered after being cut.
        assert_eq!(
            relation_sql(
                Engine::Postgres,
                "public",
                "accounts",
                &[SortKey::new(r#""id""#, true)],
                100_000
            ),
            r#"SELECT * FROM "public"."accounts" ORDER BY "id" ASC LIMIT 100000"#
        );
    }

    #[test]
    fn a_row_limit_reads_as_a_chip_not_as_a_number() {
        assert_eq!(compact_count(100), "100");
        assert_eq!(compact_count(1_000), "1K");
        assert_eq!(compact_count(100_000), "100K");
        // Every offered limit has to be labelled by this, so none can come out
        // as something like `1500`.
        for rows in explorer::ROW_LIMITS {
            assert!(compact_count(rows).len() <= 4, "{rows} is a wide label");
        }
    }

    #[test]
    fn a_snapshots_age_reads_in_one_unit() {
        assert_eq!(relative_age(0), "moments");
        assert_eq!(relative_age(59), "moments");
        assert_eq!(relative_age(60), "1m");
        assert_eq!(relative_age(3_599), "59m");
        assert_eq!(relative_age(7_200), "2h");
        assert_eq!(relative_age(86_399), "23h");
        // A cache left over a long weekend, which is exactly when its age is
        // the thing worth reading.
        assert_eq!(relative_age(3 * 86_400 + 4_000), "3d");
    }

    #[test]
    fn a_relations_statement_carries_its_sort_before_the_limit() {
        let sorted = relation_sql(
            Engine::Postgres,
            "public",
            "accounts",
            &[SortKey::new(r#""id""#, false)],
            PREVIEW_ROW_LIMIT,
        );

        assert_eq!(
            sorted,
            r#"SELECT * FROM "public"."accounts" ORDER BY "id" DESC LIMIT 1000"#
        );
        // And the sort Slate wrote is the sort its headers show.
        assert_eq!(
            sort_columns(
                Engine::Postgres,
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
    fn a_capped_snapshots_readout_says_how_much_of_the_result_it_holds() {
        // The status bar over a restored, capped grid. Without the "of" it
        // reads as 20,000 rows on screen, and the export below it as complete.
        assert_eq!(row_readout(5_000, 20_000), "5,000 of 20,000 rows");
        // Nothing was trimmed: the count it always was.
        assert_eq!(row_readout(842, 842), "842 rows");
        assert_eq!(row_readout(1, 1), "1 row");
        assert_eq!(row_readout(0, 0), "0 rows");
        // A snapshot written before `total_rows` was kept reports zero over
        // rows it can still show, and must not read as "5,000 of 0".
        assert_eq!(row_readout(5_000, 0), "0 rows");
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

    fn pending_row(sets: &[(&str, &str)], keys: &[(&str, &str)]) -> PendingRow {
        fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(column, value)| (column.to_string(), value.to_string()))
                .collect()
        }
        PendingRow {
            schema: "public".to_string(),
            table: "accounts".to_string(),
            sets: owned(sets),
            keys: owned(keys),
        }
    }

    #[test]
    fn several_pending_rows_become_one_semicolon_joined_batch() {
        let rows = vec![
            pending_row(&[("name", "Ada")], &[("id", "1")]),
            pending_row(&[("name", "Bo")], &[("id", "2")]),
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
        assert!(sql::is_generated_update(&batch));
    }

    #[test]
    fn an_engine_without_an_implicit_transaction_gets_explicit_brackets() {
        // MySQL and SQLite commit each statement on its own, so an unbracketed
        // batch could apply half the user's edits and report the failure of the
        // rest.
        let rows = vec![
            pending_row(&[("name", "Ada")], &[("id", "1")]),
            pending_row(&[("name", "Bo")], &[("id", "2")]),
        ];

        for engine in [Engine::MySql, Engine::Sqlite] {
            let batch = update_batch(engine, &rows).unwrap();
            assert!(batch.starts_with("BEGIN;\n"), "{engine:?} {batch}");
            assert!(batch.ends_with("\nCOMMIT;"), "{engine:?} {batch}");
            assert!(sql::is_generated_update(&batch), "{engine:?} {batch}");

            // One statement is already atomic, so brackets round it would be
            // ceremony the user has to read past.
            let single = update_batch(engine, &rows[..1]).unwrap();
            assert!(!single.contains("BEGIN"), "{engine:?} {single}");
            assert!(sql::is_generated_update(&single), "{engine:?} {single}");
        }

        let postgres = update_batch(Engine::Postgres, &rows).unwrap();
        assert!(!postgres.contains("BEGIN"), "{postgres}");
    }

    #[test]
    fn a_row_with_no_key_to_find_it_by_refuses_the_whole_batch() {
        let rows = vec![
            pending_row(&[("name", "Ada")], &[("id", "1")]),
            // No keys at all: sql::update_row refuses this one, since there is
            // nothing to identify the row it would touch.
            pending_row(&[("name", "Bo")], &[]),
        ];

        assert!(
            sql::update_row(
                Engine::Postgres,
                "public",
                "accounts",
                &[("name", "Bo")],
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
