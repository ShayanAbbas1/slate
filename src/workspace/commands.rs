//! The command palette and the settings surface it can open.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use super::*;

impl Workspace {
    pub(crate) fn fuzzy_open(
        &mut self,
        _: &FuzzyOpen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_palette(PaletteMode::Jump, window, cx);
    }

    pub(crate) fn command_palette(
        &mut self,
        _: &CommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_palette(PaletteMode::Commands, window, cx);
    }

    /// The same stroke again closes the palette; the other one swaps which list
    /// it is showing, so the two surfaces are one keystroke apart.
    pub(crate) fn open_palette(
        &mut self,
        mode: PaletteMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
    pub(crate) fn on_palette_event(
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
    pub(crate) fn close_palette(&mut self, cx: &mut Context<Self>) -> bool {
        if self.palette.take().is_none() {
            return false;
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.editor_needs_focus = true;
        }
        cx.notify();
        true
    }

    pub(crate) fn open_settings(
        &mut self,
        _: &OpenSettings,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_open = true;
        cx.notify();
    }

    pub(crate) fn close_settings(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.settings_open {
            return false;
        }
        self.settings_open = false;
        cx.notify();
        true
    }

    /// Every row runs through the method its button or keystroke already calls.
    /// The palette is another way in, never a second implementation.
    pub(crate) fn run_command(
        &mut self,
        command: Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
            Command::NextPage => self.turn_page(true, cx),
            Command::PreviousPage => self.turn_page(false, cx),
            Command::FilterRows => self.focus_filter(window, cx),
            Command::ClearFilter => self.clear_filter(&ClearFilter, window, cx),
            Command::NewRow => self.new_row(&NewRow, window, cx),
            Command::CloseObject(id) => self.ask_before_close(CloseTarget::Object(id), cx),
            Command::SetNull => self.set_null(&SetNull, window, cx),
            Command::DeleteRow => self.delete_row(&DeleteRow, window, cx),
            Command::ApplyEdits => self.apply_edits(&ApplyEdits, window, cx),
            Command::DiscardEdits => self.discard_edits(&DiscardEdits, window, cx),
            Command::ExportResults(format) => self.export_results(format, cx),
            Command::SwitchProfile(index) => self.activate(index, cx),
            Command::NextProfile => self.cycle_profile(1, cx),
            Command::PreviousProfile => self.cycle_profile(-1, cx),
            Command::NewConnection => self.open_connection_form(&NewConnection, window, cx),
            Command::CycleTheme => self.cycle_theme(&CycleTheme, window, cx),
            Command::PickFont(slot) => self.open_palette(PaletteMode::Font(slot), window, cx),
            Command::SetFont(slot, family) => self.set_font(slot, family, cx),
            Command::ToggleSidebar => self.toggle_sidebar(&ToggleSidebar, window, cx),
            Command::ResetEditorZoom => self.reset_editor_zoom(&ResetEditorZoom, window, cx),
            Command::OpenSettings => self.open_settings(&OpenSettings, window, cx),
        }
    }

    pub(crate) fn palette_next(
        &mut self,
        _: &PaletteNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_palette_selection(1, window, cx);
    }

    pub(crate) fn palette_previous(
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
    pub(crate) fn move_palette_selection(
        &mut self,
        step: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
}
