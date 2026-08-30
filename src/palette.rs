//! `cmd+p` and `cmd+shift+p`: one list and one matcher, two jobs (spec §3.4).
//!
//! The jump is flat over everything the active profile can open, so a table
//! three schemas down is a few keystrokes away without touching the tree. The
//! command palette is over verbs, and it offers only the verbs that apply to
//! what is on screen — a Run on a tab with no buffer is a row to read past.
//!
//! Neither surface decides anything. A row carries a [`Command`], the workspace
//! runs it through the same methods the buttons and keystrokes call, and the
//! palette is gone by the time it happens.

use gpui::{App, Context, IntoElement, ParentElement, SharedString, Styled, Task, Window, div, px};
use gpui_component::{
    IndexPath,
    list::{ListDelegate, ListItem, ListState},
};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};

use crate::{
    CatalogState, ObjectBody, Profile, Tab, Workspace,
    db::{RelationKind, RoutineKind},
    explorer::{ExplorerTarget, ObjectKind},
    export::Format,
    icons::icon,
    object_icon, routine_name, row_icon,
    theme::{layout, theme},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `cmd+p` — everything this profile can open, in one flat list.
    Jump,
    /// `cmd+shift+p` — the verbs that apply right now.
    Commands,
    /// The statements this profile has run, reached from the command palette.
    History,
}

/// What a row does when it is confirmed.
///
/// Every variant names something the workspace already does for a button or a
/// keystroke: the palette is another way to reach them, not a second
/// implementation of them.
#[derive(Clone)]
pub enum Command {
    OpenObject(ExplorerTarget),
    OpenQuery(String),
    OpenScratch,
    NewQuery,
    RunQuery,
    SaveQuery,
    RenameQuery,
    /// Open the history list. The one command that puts the palette back up
    /// rather than doing something behind it.
    QueryHistory,
    /// Put a statement that has already been run back in the buffer.
    RecallStatement(String),
    ShowStructure(bool),
    RefreshRelation(u64),
    CloseObject(u64),
    ApplyEdits,
    DiscardEdits,
    /// The format here only picks the extension the save dialog suggests. What
    /// the file is written as is read back off the path the user confirmed, so
    /// these two rows are one code path — see `export::Format::for_path`.
    ExportResults(Format),
    SwitchProfile(usize),
    NewConnection,
    CycleTheme,
    ResetEditorZoom,
}

struct Item {
    /// What the matcher scores, and what the row reads as. One string for both,
    /// so nothing can be found by text that is not on screen.
    label: String,
    /// Muted, at the far end: what kind of object this is, or the stroke that
    /// does the same job without the palette.
    hint: SharedString,
    icon: &'static str,
    command: Command,
}

impl Item {
    fn command(label: &str, hint: &'static str, icon: &'static str, command: Command) -> Self {
        Self {
            label: label.to_string(),
            hint: hint.into(),
            icon,
            command,
        }
    }
}

pub struct Palette {
    mode: Mode,
    items: Vec<Item>,
    /// Indices into `items`, best first. The list renders through this, so the
    /// order on screen is the matcher's order.
    matched: Vec<usize>,
    matcher: Matcher,
}

impl Palette {
    pub fn new(mode: Mode, workspace: &Workspace, cx: &App) -> Self {
        let items = match workspace.profile() {
            Some(profile) => match mode {
                Mode::Jump => jump_items(profile),
                Mode::Commands => command_items(workspace, profile, cx),
                Mode::History => history_items(profile),
            },
            None => Vec::new(),
        };
        Self {
            mode,
            matched: (0..items.len()).collect(),
            items,
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn len(&self) -> usize {
        self.matched.len()
    }

    pub fn command(&self, row: usize) -> Option<&Command> {
        let index = *self.matched.get(row)?;
        Some(&self.items.get(index)?.command)
    }

    pub fn placeholder(&self) -> &'static str {
        match self.mode {
            Mode::Jump => "Go to a table, view, routine or saved query…",
            Mode::Commands => "Run a command…",
            Mode::History => "Recall a statement you have run…",
        }
    }
}

impl ListDelegate for Palette {
    type Item = ListItem;

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.matched.len()
    }

    fn perform_search(
        &mut self,
        query: &str,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.matched = matches(&self.items, query, &mut self.matcher);
        Task::ready(())
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let t = *theme(cx);
        let item = self.items.get(*self.matched.get(ix.row)?)?;

        Some(
            ListItem::new(ix.row)
                .rounded(px(layout::RADIUS_CONTROL))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_SM))
                        .w_full()
                        .min_w_0()
                        .child(row_icon(t, item.icon))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(item.label.clone()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(layout::TEXT_XS))
                                .text_color(t.text_faint)
                                .child(item.hint.clone()),
                        ),
                ),
        )
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        div()
            .p(px(layout::SPACE_MD))
            .text_color(theme(cx).text_muted)
            .child("No matches.")
    }

    fn set_selected_index(
        &mut self,
        _: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) {
    }
}

/// Everything the active profile can open: its buffers first, because they are
/// what somebody wrote, then the database's own objects.
fn jump_items(profile: &Profile) -> Vec<Item> {
    let session = &profile.session;
    let mut items = vec![Item::command(
        "New Query",
        "scratch",
        icon::SCRATCH_QUERY,
        Command::OpenScratch,
    )];

    items.extend(session.saved_queries.iter().map(|name| Item {
        label: name.clone(),
        hint: "query".into(),
        icon: icon::SAVED_QUERY,
        command: Command::OpenQuery(name.clone()),
    }));

    let CatalogState::Loaded(catalog) = &profile.catalog else {
        return items;
    };
    for (schema_index, schema) in catalog.schemas.iter().enumerate() {
        items.extend(
            schema
                .relations
                .iter()
                .enumerate()
                .map(|(relation_index, relation)| {
                    let kind = ObjectKind::Relation(relation.kind);
                    Item {
                        // Qualified, so a name that appears in three schemas is three
                        // rows that can be told apart, and "pub acc" reaches one of them.
                        label: format!("{}.{}", schema.name, relation.name),
                        hint: kind_label(kind).into(),
                        icon: object_icon(kind),
                        command: Command::OpenObject(ExplorerTarget::Relation {
                            schema_index,
                            relation_index,
                        }),
                    }
                }),
        );
        items.extend(
            schema
                .routines
                .iter()
                .enumerate()
                .map(|(routine_index, routine)| {
                    let kind = ObjectKind::Routine(routine.kind);
                    Item {
                        label: format!("{}.{}", schema.name, routine_name(routine)),
                        hint: kind_label(kind).into(),
                        icon: object_icon(kind),
                        command: Command::OpenObject(ExplorerTarget::Routine {
                            schema_index,
                            routine_index,
                        }),
                    }
                }),
        );
    }
    items
}

/// Every statement this profile has run, newest first.
fn history_items(profile: &Profile) -> Vec<Item> {
    profile
        .session
        .history
        .iter()
        .map(|sql| Item {
            label: one_line(sql),
            hint: "".into(),
            icon: icon::HISTORY,
            command: Command::RecallStatement(sql.clone()),
        })
        .collect()
}

/// A statement as a row: one line, however many it was written across. The
/// label is what the matcher scores as well as what the row reads as, so
/// collapsing the whitespace is also what makes `select from accounts` find a
/// statement whose `FROM` was on its own line.
fn one_line(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The verbs, filtered to the ones that mean something from where the user is
/// standing. A palette that lists what it cannot do is a palette to read past.
fn command_items(workspace: &Workspace, profile: &Profile, cx: &App) -> Vec<Item> {
    let session = &profile.session;
    let runnable = session.editor(session.active).is_some();
    let mut items = vec![Item::command(
        "New query",
        "⌘T",
        icon::SCRATCH_QUERY,
        Command::NewQuery,
    )];

    if runnable {
        items.push(Item::command(
            "Run query",
            "⌘↩",
            icon::RUN,
            Command::RunQuery,
        ));
        if session.open_query.is_some() {
            items.push(Item::command(
                "Rename query",
                "",
                icon::RENAME,
                Command::RenameQuery,
            ));
        } else {
            items.push(Item::command(
                "Save query",
                "⌘S",
                icon::SAVE,
                Command::SaveQuery,
            ));
        }
    }

    // Not gated on the query tab being in front: recalling a statement brings
    // it forward, which is where the statement is going anyway.
    if !session.history.is_empty() {
        items.push(Item::command(
            "Query history",
            "",
            icon::HISTORY,
            Command::QueryHistory,
        ));
    }

    if let Some(tab) = session.active_object() {
        if let ObjectBody::Relation {
            showing_structure, ..
        } = &tab.body
        {
            items.push(if *showing_structure {
                Item::command("Show data", "", icon::TABLE, Command::ShowStructure(false))
            } else {
                Item::command(
                    "Show structure",
                    "",
                    icon::STRUCTURE,
                    Command::ShowStructure(true),
                )
            });
            items.push(Item::command(
                "Refresh rows",
                "",
                icon::RUN,
                Command::RefreshRelation(tab.id),
            ));
        }
        items.push(Item::command(
            "Close tab",
            "⌘W",
            icon::CLOSE,
            Command::CloseObject(tab.id),
        ));
    }

    // Only over a grid that has a result set behind it. A surface that has run
    // nothing has nothing to write out.
    if workspace.has_results(cx) {
        items.push(Item::command(
            "Export results as CSV",
            "",
            icon::SAVE,
            Command::ExportResults(Format::Csv),
        ));
        items.push(Item::command(
            "Export results as JSON",
            "",
            icon::SAVE,
            Command::ExportResults(Format::Json),
        ));
    }

    // Only while there is something to write back, for the reason the footer's
    // pair of buttons is conditional too.
    if workspace.has_pending_edits(cx) {
        items.push(Item::command(
            "Apply edits",
            "",
            icon::SAVE,
            Command::ApplyEdits,
        ));
        items.push(Item::command(
            "Discard edits",
            "",
            icon::DELETE,
            Command::DiscardEdits,
        ));
    }

    items.extend(
        workspace
            .profiles
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != workspace.active)
            .map(|(index, other)| Item {
                label: format!("Switch to {}", other.name),
                hint: "⌃⇥".into(),
                icon: icon::DATABASE,
                command: Command::SwitchProfile(index),
            }),
    );

    items.push(Item::command(
        "New connection",
        "⇧⌘N",
        icon::PLUS,
        Command::NewConnection,
    ));
    items.push(Item::command(
        "Cycle theme",
        "⇧⌘T",
        icon::SWITCHER,
        Command::CycleTheme,
    ));
    if matches!(session.active, Tab::Query) {
        items.push(Item::command(
            "Reset editor zoom",
            "⌘0",
            icon::SEARCH,
            Command::ResetEditorZoom,
        ));
    }
    items
}

fn kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Relation(RelationKind::Table) => "table",
        ObjectKind::Relation(RelationKind::PartitionedTable) => "partitioned table",
        ObjectKind::Relation(RelationKind::View) => "view",
        ObjectKind::Relation(RelationKind::MaterializedView) => "materialized view",
        ObjectKind::Relation(RelationKind::ForeignTable) => "foreign table",
        ObjectKind::Routine(RoutineKind::Function) => "function",
        ObjectKind::Routine(RoutineKind::Procedure) => "procedure",
    }
}

/// Score every label against the query and keep the survivors, best first.
///
/// An empty query scores everything at zero, and the sort is stable, so the
/// unfiltered list is the list in the order it was built.
fn matches(items: &[Item], query: &str, matcher: &mut Matcher) -> Vec<usize> {
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buffer = Vec::new();
    let mut scored = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let label = Utf32Str::new(&item.label, &mut buffer);
            pattern.score(label, matcher).map(|score| (index, score))
        })
        .collect::<Vec<_>>();
    scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
    scored.into_iter().map(|(index, _)| index).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(labels: &[&str]) -> Vec<Item> {
        labels
            .iter()
            .map(|label| Item::command(label, "", icon::TABLE, Command::NewQuery))
            .collect()
    }

    fn matched(labels: &[&str], query: &str) -> Vec<String> {
        let items = items(labels);
        let mut matcher = Matcher::new(Config::DEFAULT);
        matches(&items, query, &mut matcher)
            .into_iter()
            .map(|index| items[index].label.clone())
            .collect()
    }

    #[test]
    fn an_empty_query_keeps_every_row_in_the_order_it_was_built() {
        let labels = ["public.accounts", "public.events", "analytics.events"];
        assert_eq!(matched(&labels, ""), labels);
    }

    #[test]
    fn a_query_drops_what_it_does_not_match_and_ranks_what_it_does() {
        let labels = ["analytics.events", "public.accounts", "public.account_log"];
        // The gaps in the subsequence are what the ranking is about: the whole
        // word beats the same letters spread across a longer name.
        assert_eq!(
            matched(&labels, "account"),
            ["public.accounts", "public.account_log"]
        );
        assert!(matched(&labels, "zzz").is_empty());
    }

    #[test]
    fn a_statement_written_across_lines_reads_and_matches_as_one() {
        let sql = "SELECT *\n  FROM accounts\n WHERE id = 1;";
        assert_eq!(one_line(sql), "SELECT * FROM accounts WHERE id = 1;");
        assert_eq!(matched(&[&one_line(sql)], "from accounts").len(), 1);
    }

    #[test]
    fn a_schema_prefix_narrows_to_one_schema() {
        let labels = ["analytics.events", "public.events"];
        assert_eq!(matched(&labels, "pub ev"), ["public.events"]);
    }
}
