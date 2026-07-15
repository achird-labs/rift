//! Keyboard event handling for App

use super::*;

impl App {
    /// Handle keyboard input
    pub async fn handle_key_event(&mut self, key: KeyEvent) {
        // Handle overlays first
        match &self.overlay.clone() {
            Overlay::Errors => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('L') | KeyCode::Char('q') => {
                        self.overlay = Overlay::None;
                        self.errors_scroll = 0;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.errors_scroll = self.errors_scroll.saturating_sub(1);
                    }
                    // Bounded by the entry count, so scrolling cannot run past the last error.
                    KeyCode::Down | KeyCode::Char('j')
                        if self.errors_scroll + 1 < self.errors.len() =>
                    {
                        self.errors_scroll += 1;
                    }
                    _ => {}
                }
                return;
            }
            Overlay::Help => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('?') => {
                        self.overlay = Overlay::None;
                        self.help_scroll = 0;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.help_scroll = self.help_scroll.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if self.help_scroll < self.help_max_scroll {
                            self.help_scroll += 1;
                        }
                    }
                    KeyCode::PageUp => {
                        self.help_scroll = self.help_scroll.saturating_sub(10);
                    }
                    KeyCode::PageDown => {
                        self.help_scroll = (self.help_scroll + 10).min(self.help_max_scroll);
                    }
                    KeyCode::Home => {
                        self.help_scroll = 0;
                    }
                    KeyCode::End => {
                        self.help_scroll = self.help_max_scroll;
                    }
                    _ => {}
                }
                return;
            }
            Overlay::Confirm { .. } => match key.code {
                KeyCode::Enter => {
                    self.execute_pending_action().await;
                    return;
                }
                KeyCode::Esc => {
                    self.overlay = Overlay::None;
                    return;
                }
                _ => return,
            },
            Overlay::Error { .. } => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                    self.overlay = Overlay::None;
                }
                return;
            }
            Overlay::Input { action, .. } => {
                self.handle_input_event(key, action.clone()).await;
                return;
            }
            Overlay::Export { content, port, .. } => {
                match key.code {
                    KeyCode::Esc => {
                        self.overlay = Overlay::None;
                        self.export_scroll_offset = 0;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.export_scroll_offset = self.export_scroll_offset.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let max_scroll = content.lines().count().saturating_sub(10) as u16;
                        self.export_scroll_offset = (self.export_scroll_offset + 1).min(max_scroll);
                    }
                    KeyCode::PageUp => {
                        self.export_scroll_offset = self.export_scroll_offset.saturating_sub(10);
                    }
                    KeyCode::PageDown => {
                        let max_scroll = content.lines().count().saturating_sub(10) as u16;
                        self.export_scroll_offset =
                            (self.export_scroll_offset + 10).min(max_scroll);
                    }
                    KeyCode::Char('s') if port.is_some() => {
                        let content_clone = content.clone();
                        let port_val = port.unwrap();
                        self.export_scroll_offset = 0;
                        self.show_save_dialog(content_clone, port_val);
                    }
                    KeyCode::Char('c') if port.is_some() => {
                        let content_clone = content.clone();
                        self.copy_to_clipboard(&content_clone);
                    }
                    _ => {}
                }
                return;
            }
            Overlay::FilePathInput { action, .. } => {
                self.handle_file_path_input(key, action.clone()).await;
                return;
            }
            Overlay::Success { .. } => {
                self.overlay = Overlay::None;
                return;
            }
            Overlay::ValidationResult { action, .. } => {
                self.handle_validation_overlay_event(key, action.clone())
                    .await;
                return;
            }
            Overlay::None => {}
        }

        // Handle editor mode
        if matches!(self.view, View::StubEdit { .. }) {
            self.handle_editor_event(key).await;
            return;
        }

        // Handle search mode
        if self.search_active {
            self.handle_search_input(key);
            return;
        }

        // Global keys
        match key.code {
            // `L` for error Log. NOT `e`: the global block runs before the view dispatch and
            // returns, so binding `e` here would shadow the view-local `e` (export-all, stub-edit).
            KeyCode::Char('L') => {
                self.overlay = Overlay::Errors;
                self.errors_scroll = 0;
                return;
            }
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
                self.help_scroll = 0;
                // Help text has ~75 lines, set max_scroll based on typical terminal height
                self.help_max_scroll = 50;
                return;
            }
            KeyCode::Char('/') => {
                self.search_active = true;
                self.search_query.clear();
                return;
            }
            KeyCode::Char('q') => {
                if matches!(self.view, View::ImposterList) {
                    self.should_quit = true;
                } else {
                    self.go_back();
                }
                return;
            }
            KeyCode::Esc => {
                self.go_back();
                return;
            }
            KeyCode::Char('r') => {
                self.refresh().await;
                return;
            }
            KeyCode::Char('T') => {
                self.cycle_theme();
                return;
            }
            _ => {}
        }

        // View-specific keys
        match self.view.clone() {
            View::ImposterList => self.handle_imposter_list_event(key).await,
            View::ImposterDetail { .. } => self.handle_imposter_detail_event(key).await,
            View::StubDetail { .. } => self.handle_stub_detail_event(key).await,
            View::RequestDetail { .. } => {}
            View::Config => self.handle_config_event(key).await,
            View::Metrics => {}
            View::StubEdit { .. } => {}
        }
    }

    async fn handle_config_event(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('r') {
            match self.client.get_config().await {
                Ok(cfg) => {
                    self.server_config = Some(cfg);
                    self.set_status("Config refreshed".to_string(), StatusLevel::Success);
                }
                Err(e) => {
                    self.set_status(format!("Failed to load config: {e}"), StatusLevel::Error)
                }
            }
        }
    }

    async fn handle_imposter_list_event(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_previous(),
            KeyCode::Enter => self.enter_imposter_detail().await,
            KeyCode::Char('n') => self.show_create_imposter(),
            KeyCode::Char('p') => self.show_create_proxy_imposter(),
            KeyCode::Char('d') => self.confirm_delete_imposter(),
            KeyCode::Char('t') => self.toggle_imposter().await,
            KeyCode::Char('m') => self.navigate(View::Metrics),
            KeyCode::Char('C') => self.open_config_view().await,
            KeyCode::Char('i') => self.show_import_file_dialog(),
            KeyCode::Char('I') => self.show_import_folder_dialog(),
            KeyCode::Char('e') => self.show_export_all_dialog(),
            KeyCode::Char('E') => self.show_export_folder_dialog(),
            _ => {}
        }
    }

    async fn open_config_view(&mut self) {
        self.is_loading = true;
        match self.client.get_config().await {
            Ok(cfg) => {
                self.server_config = Some(cfg);
                self.navigate(View::Config);
            }
            Err(e) => self.set_status(format!("Failed to load config: {e}"), StatusLevel::Error),
        }
        self.is_loading = false;
    }

    async fn handle_imposter_detail_event(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_previous(),
            KeyCode::Tab => self.toggle_focus(),
            KeyCode::Char('a') => self.start_stub_create(),
            KeyCode::Char('e') => self.start_stub_edit(),
            KeyCode::Char('d') => self.confirm_delete_stub(),
            KeyCode::Char('c') => self.confirm_clear_requests(),
            KeyCode::Char('C') => self.confirm_clear_proxy_responses(),
            KeyCode::Char('x') => self.export_imposter(true).await,
            KeyCode::Char('X') => self.export_imposter(false).await,
            KeyCode::Char('A') => self.confirm_apply_recorded_stubs(),
            KeyCode::Char('t') => self.toggle_imposter().await,
            KeyCode::Char('y') => self.copy_stub_as_curl(),
            KeyCode::Char('[') => self.reorder_stub(-1).await,
            KeyCode::Char(']') => self.reorder_stub(1).await,
            KeyCode::Char('D') => self.duplicate_stub().await,
            KeyCode::Enter => {
                if let View::ImposterDetail { port } = self.view {
                    match self.focus {
                        FocusArea::Left => {
                            if let Some(idx) = self.stub_list_state.selected() {
                                self.navigate(View::StubDetail { port, index: idx });
                            }
                        }
                        FocusArea::Right => {
                            if let Some(idx) = self.request_list_state.selected() {
                                self.navigate(View::RequestDetail { port, index: idx });
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    async fn handle_stub_detail_event(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('e') => self.start_stub_edit(),
            KeyCode::Char('d') => self.confirm_delete_stub(),
            KeyCode::Char('y') => self.copy_stub_as_curl(),
            KeyCode::Char('D') => self.duplicate_stub().await,
            _ => {}
        }
    }

    pub(super) async fn handle_editor_event(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('s') => {
                    self.save_stub().await;
                    return;
                }
                KeyCode::Char('f') => {
                    if let Some(editor) = &mut self.stub_editor {
                        editor.format();
                    }
                    return;
                }
                KeyCode::Char('l') => {
                    // Show full lint validation results
                    self.show_editor_validation();
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => {
                self.cancel_stub_edit();
            }
            _ => {
                // Get the action first
                let action = if let Some(editor) = &mut self.stub_editor {
                    editor.handle_key(key)
                } else {
                    None
                };

                // Handle clipboard actions (need separate borrows)
                match action {
                    Some(EditorAction::Copy(text)) | Some(EditorAction::Cut(text)) => {
                        self.copy_to_clipboard(&text);
                    }
                    Some(EditorAction::PasteRequest) => {
                        if let Some(text) = self.paste_from_clipboard()
                            && let Some(editor) = &mut self.stub_editor
                        {
                            editor.editor.set_yank_text(text.clone());
                            editor.editor.input(ratatui_textarea::Input {
                                key: ratatui_textarea::Key::Char('y'),
                                ctrl: true,
                                alt: false,
                                shift: false,
                            });
                        }
                    }
                    None => {}
                }

                // Validate after any changes
                if let Some(editor) = &mut self.stub_editor {
                    editor.validate();
                }
            }
        }
    }

    pub(super) async fn handle_input_event(&mut self, key: KeyEvent, action: InputAction) {
        match action {
            InputAction::CreateImposter => self.handle_create_imposter_input(key).await,
            InputAction::CreateProxyImposter => self.handle_create_proxy_input(key).await,
        }
    }

    async fn handle_create_imposter_input(&mut self, key: KeyEvent) {
        // Handle Ctrl+V paste
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('v') {
            if let Some(text) = self.paste_from_clipboard() {
                match self.input_state.focus_field {
                    0 => {
                        // Port: only paste digits
                        let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
                        self.input_state.port.push_str(&digits);
                    }
                    1 => self.input_state.name.push_str(&text),
                    2 => self.input_state.protocol.push_str(&text),
                    _ => {}
                }
            }
            return;
        }

        match key.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Enter => self.create_imposter().await,
            KeyCode::Tab => self.input_state.focus_field = (self.input_state.focus_field + 1) % 3,
            KeyCode::BackTab => {
                self.input_state.focus_field = if self.input_state.focus_field == 0 {
                    2
                } else {
                    self.input_state.focus_field - 1
                };
            }
            KeyCode::Backspace => match self.input_state.focus_field {
                0 => {
                    self.input_state.port.pop();
                }
                1 => {
                    self.input_state.name.pop();
                }
                2 => {
                    self.input_state.protocol.pop();
                }
                _ => {}
            },
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                match self.input_state.focus_field {
                    0 => {
                        if c.is_ascii_digit() {
                            self.input_state.port.push(c);
                        }
                    }
                    1 => {
                        self.input_state.name.push(c);
                    }
                    2 => {
                        self.input_state.protocol.push(c);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    async fn handle_create_proxy_input(&mut self, key: KeyEvent) {
        // Handle Ctrl+V paste
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('v') {
            if let Some(text) = self.paste_from_clipboard() {
                match self.input_state.focus_field {
                    0 => self.input_state.target_url.push_str(&text),
                    1 => {
                        // Port: only paste digits
                        let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
                        self.input_state.port.push_str(&digits);
                    }
                    2 => self.input_state.name.push_str(&text),
                    _ => {}
                }
            }
            return;
        }

        match key.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Enter => self.create_proxy_imposter().await,
            KeyCode::Tab => self.input_state.focus_field = (self.input_state.focus_field + 1) % 4,
            KeyCode::BackTab => {
                self.input_state.focus_field = if self.input_state.focus_field == 0 {
                    3
                } else {
                    self.input_state.focus_field - 1
                };
            }
            KeyCode::Left if self.input_state.focus_field == 3 => {
                self.input_state.proxy_mode = if self.input_state.proxy_mode == 0 {
                    2
                } else {
                    self.input_state.proxy_mode - 1
                };
            }
            KeyCode::Right if self.input_state.focus_field == 3 => {
                self.input_state.proxy_mode = (self.input_state.proxy_mode + 1) % 3;
            }
            KeyCode::Backspace => match self.input_state.focus_field {
                0 => {
                    self.input_state.target_url.pop();
                }
                1 => {
                    self.input_state.port.pop();
                }
                2 => {
                    self.input_state.name.pop();
                }
                _ => {}
            },
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                match self.input_state.focus_field {
                    0 => {
                        self.input_state.target_url.push(c);
                    }
                    1 => {
                        if c.is_ascii_digit() {
                            self.input_state.port.push(c);
                        }
                    }
                    2 => {
                        self.input_state.name.push(c);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    pub(super) async fn handle_file_path_input(&mut self, key: KeyEvent, action: FileAction) {
        // Handle Ctrl+V paste
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('v') {
            if let Some(text) = self.paste_from_clipboard() {
                // Insert pasted text at cursor position
                for c in text.chars() {
                    self.input_state
                        .file_path
                        .insert(self.input_state.cursor_pos, c);
                    self.input_state.cursor_pos += 1;
                }
            }
            return;
        }

        match key.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Enter => {
                let path = self.input_state.file_path.clone();
                if path.is_empty() {
                    return;
                }
                match action {
                    FileAction::SaveExport { content, .. } => {
                        self.save_to_file(&path, &content).await
                    }
                    FileAction::ImportFile => self.import_from_file(&path).await,
                    FileAction::ImportFolder => self.import_from_folder(&path).await,
                    FileAction::ExportAll => self.export_all_to_file(&path).await,
                    FileAction::ExportToFolder => self.export_to_folder(&path).await,
                }
            }
            KeyCode::Left => {
                if self.input_state.cursor_pos > 0 {
                    self.input_state.cursor_pos -= 1;
                }
            }
            KeyCode::Right => {
                if self.input_state.cursor_pos < self.input_state.file_path.len() {
                    self.input_state.cursor_pos += 1;
                }
            }
            KeyCode::Home => self.input_state.cursor_pos = 0,
            KeyCode::End => self.input_state.cursor_pos = self.input_state.file_path.len(),
            KeyCode::Backspace => {
                if self.input_state.cursor_pos > 0 {
                    self.input_state.cursor_pos -= 1;
                    self.input_state
                        .file_path
                        .remove(self.input_state.cursor_pos);
                }
            }
            KeyCode::Delete => {
                if self.input_state.cursor_pos < self.input_state.file_path.len() {
                    self.input_state
                        .file_path
                        .remove(self.input_state.cursor_pos);
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input_state
                    .file_path
                    .insert(self.input_state.cursor_pos, c);
                self.input_state.cursor_pos += 1;
            }
            _ => {}
        }
    }

    /// Handle validation result overlay events
    pub(super) async fn handle_validation_overlay_event(
        &mut self,
        key: KeyEvent,
        action: ValidationAction,
    ) {
        if let Overlay::ValidationResult { report, .. } = &self.overlay {
            let total_issues = report.issues.len() as u16;
            match key.code {
                KeyCode::Esc => {
                    self.overlay = Overlay::None;
                    self.validation_scroll_offset = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.validation_scroll_offset = self.validation_scroll_offset.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let max_scroll = total_issues.saturating_sub(5);
                    self.validation_scroll_offset =
                        (self.validation_scroll_offset + 1).min(max_scroll);
                }
                KeyCode::PageUp => {
                    self.validation_scroll_offset = self.validation_scroll_offset.saturating_sub(5);
                }
                KeyCode::PageDown => {
                    let max_scroll = total_issues.saturating_sub(5);
                    self.validation_scroll_offset =
                        (self.validation_scroll_offset + 5).min(max_scroll);
                }
                KeyCode::Enter => {
                    // Only allow proceeding if action supports it
                    if let ValidationAction::ProceedWithImport { content, .. } = &action {
                        let content = content.clone();
                        self.overlay = Overlay::None;
                        self.validation_scroll_offset = 0;
                        self.do_import(&content).await;
                    }
                }
                _ => {}
            }
        }
    }
}
