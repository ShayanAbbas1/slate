//! Every action the keymap can dispatch.
//!
//! The ones carrying a field say which thing they act on, because the surface
//! they come from cannot always be inferred from focus. The rest are the
//! zero-field verbs `actions!` generates.

use gpui::{Action, actions};
use serde::Deserialize;

use crate::filter::Operator;

/// A header click. The column is the one in the grid; which statement it
/// belongs to is whatever surface is in front, because that is the grid the
/// click came from.
#[derive(Clone, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = slate, no_json)]
pub(crate) struct SortColumn {
    pub(crate) column: usize,
}

/// The column a filter bar narrows on, picked from the bar's dropdown. The bar
/// is named by position in the stack, which is how every one of these reaches
/// it: the stack is what the user is pointing at.
#[derive(Clone, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = slate, no_json)]
pub(crate) struct SetFilterColumn {
    pub(crate) row: usize,
    pub(crate) column: String,
}

/// Turn a bar into one the user writes SQL into, which has no column and no
/// operator left to pick.
#[derive(Clone, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = slate, no_json)]
pub(crate) struct SetFilterRaw {
    pub(crate) row: usize,
}

#[derive(Clone, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = slate, no_json)]
pub(crate) struct SetFilterOperator {
    pub(crate) row: usize,
    pub(crate) operator: Operator,
}

/// Flip how a bar joins to the bar above it.
#[derive(Clone, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = slate, no_json)]
pub(crate) struct ToggleFilterJoin {
    pub(crate) row: usize,
}

#[derive(Clone, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = slate, no_json)]
pub(crate) struct RemoveFilter {
    pub(crate) row: usize,
}

/// How many rows a relation's preview asks for. Slate's own statement carries
/// the limit, so the only thing to say is the number.
#[derive(Clone, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = slate, no_json)]
pub(crate) struct SetRowLimit {
    pub(crate) rows: usize,
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
        NextTab,
        PreviousTab,
        NewConnection,
        ZoomEditorIn,
        ZoomEditorOut,
        ResetEditorZoom,
        NextPage,
        PreviousPage,
        ClearFilter,
        AddFilter,
        ToggleNextJoin,
        NewRow,
        EditCell,
        CopyCell,
        SetNull,
        FollowForeignKey,
        DeleteRow,
        ApplyEdits,
        DiscardEdits,
        FuzzyOpen,
        CommandPalette,
        PaletteNext,
        PalettePrevious,
        CloseTab,
        ToggleSidebar,
        AcceptCompletion,
        OpenSettings,
        Quit,
    ]
);
