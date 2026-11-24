use lune_terminal::CellPosition;
use winit::event::KeyEvent;
use winit::keyboard::{Key, NamedKey};

use super::{Command, Mode, normalize};
use crate::desktop::{Client, selection::Selection, shortcuts};

impl Client {
    pub(in crate::desktop) fn toggle_vi(&mut self) {
        if self.vi.is_some() {
            self.leave_vi();
            return;
        }
        if let Some(button) = self.pressed.take() {
            self.mouse_event(lune_terminal::MouseEvent::Release(button));
        }
        self.clear_selection();
        let screen = self.session.screen();
        let mut vi = Mode::new(screen.active_buffer(), screen.viewport(), self.vi_columns());
        vi.search = self.vi_search.take();
        self.vi = Some(vi);
        log::debug!("Entered Vi mode");
        self.window.set_ime_allowed(false);
        self.set_preedit(String::new());
        self.redraw_vi();
    }

    pub(in crate::desktop) fn leave_vi(&mut self) {
        self.vi_search = self.vi.take().and_then(|vi| vi.search);
        log::debug!("Left Vi mode");
        self.window.set_ime_allowed(true);
        self.set_preedit(String::new());
        self.follow_output();
        self.redraw_vi();
    }

    pub(in crate::desktop) fn vi_key(&mut self, event: &KeyEvent) {
        let vi = self.vi.as_ref().unwrap();
        if vi.prompt.is_some() {
            self.vi_search_key(event);
            return;
        }
        let command = match shortcuts::vi_binding(event, self.modifiers, vi.buffer) {
            Some(binding) => match binding.action(event.repeat) {
                Some(command) => Some(command),
                None => return,
            },
            None if matches!(
                event.logical_key,
                Key::Named(
                    NamedKey::Shift
                        | NamedKey::Control
                        | NamedKey::Alt
                        | NamedKey::AltGraph
                        | NamedKey::Super
                        | NamedKey::CapsLock
                )
            ) =>
            {
                return;
            }
            None => None,
        };
        let columns = self.vi_columns();
        let viewport = self.session.screen().viewport();
        let vi = self.vi.as_mut().unwrap();
        let Some(command) = vi.resolve(command) else {
            return;
        };
        let steps = match command {
            Command::Move(motion) => vi.move_cursor(motion, viewport, columns),
            Command::Scroll(steps) => {
                self.scroll_history(steps);
                self.redraw_vi();
                return;
            }
            Command::Visual(kind) => {
                vi.toggle_visual(kind);
                0
            }
            Command::WordObject(around) => {
                vi.select_word(viewport.source(), columns, around);
                0
            }
            Command::Yank => {
                if self.copy_pending || self.selection.is_none() {
                    return;
                }
                self.copy();
                self.vi.as_mut().unwrap().clear_visual();
                0
            }
            Command::Exit => {
                self.leave_vi();
                return;
            }
            Command::Cancel => {
                vi.clear_visual();
                0
            }
            Command::Search(direction) => {
                vi.start_search(direction);
                self.window.set_ime_allowed(true);
                self.update_ime_area();
                self.redraw_vi();
                return;
            }
            Command::RepeatSearch(reverse) => {
                vi.repeat_search(viewport.source(), columns, reverse);
                0
            }
            Command::TopPrefix | Command::InnerOrExit | Command::AroundOrExit => unreachable!(),
        };
        if steps != 0 {
            self.session.scroll_history(steps);
        }
        self.reveal_vi_cursor();
        self.update_vi_selection();
        self.redraw_vi();
    }

    fn vi_search_key(&mut self, event: &KeyEvent) {
        let columns = self.vi_columns();
        let vi = self.vi.as_mut().unwrap();
        if matches!(
            event.logical_key,
            Key::Named(NamedKey::Enter | NamedKey::Escape)
        ) {
            if matches!(event.logical_key, Key::Named(NamedKey::Enter)) {
                vi.confirm_search(self.session.screen().view(), columns);
            } else {
                vi.prompt = None;
            }
            self.window.set_ime_allowed(false);
            self.set_preedit(String::new());
            self.reveal_vi_cursor();
            self.update_vi_selection();
        } else {
            let prompt = vi.prompt.as_mut().unwrap();
            match &event.logical_key {
                Key::Named(NamedKey::Backspace) => prompt.backspace(),
                _ if self.modifiers.control_key()
                    || self.modifiers.alt_key()
                    || self.modifiers.super_key() =>
                {
                    return;
                }
                Key::Named(NamedKey::Space) => prompt.push(" "),
                Key::Character(text) => prompt.push(text),
                _ => return,
            }
            self.update_ime_area();
        }
        self.redraw_vi();
    }

    pub(in crate::desktop) fn refresh_vi(&mut self) {
        let columns = self.vi_columns();
        let screen = self.session.screen();
        let Some(vi) = &mut self.vi else { return };
        if screen.active_buffer() != vi.buffer {
            self.leave_vi();
            return;
        }
        let old_cursor = vi.cursor;
        let old_start = screen.viewport().range().start;
        vi.rebase(screen.view(), columns);
        let moved = old_cursor != vi.cursor;
        self.reveal_vi_cursor();
        if moved || old_start != self.session.screen().viewport().range().start {
            self.redraw_vi();
        }
    }

    pub(in crate::desktop) fn reset_vi_cursor(&mut self, row: usize) {
        let columns = self.vi_columns();
        let viewport = self.session.screen().viewport();
        if let Some(vi) = &mut self.vi {
            vi.clear_visual();
            vi.rebase(viewport.source(), columns);
            vi.set_cursor(normalize(
                viewport.source(),
                CellPosition {
                    row: viewport.range().start + row.min(viewport.rows() - 1),
                    column: vi.cursor.column,
                },
                columns,
            ));
        }
    }

    pub(in crate::desktop) fn scroll_vi_cursor(&mut self) {
        let columns = self.vi_columns();
        let viewport = self.session.screen().viewport();
        if let Some(vi) = &mut self.vi {
            let rows = viewport.range();
            if !rows.contains(&vi.cursor.row) {
                vi.set_cursor(normalize(
                    viewport.source(),
                    CellPosition {
                        row: vi.cursor.row.clamp(rows.start, rows.end - 1),
                        column: vi.cursor.column,
                    },
                    columns,
                ));
                self.update_vi_selection();
            }
        }
    }

    fn reveal_vi_cursor(&mut self) {
        let cursor = self.vi.as_ref().unwrap().cursor;
        let rows = self.session.screen().viewport().range();
        if cursor.row < rows.start {
            self.session
                .scroll_history((rows.start - cursor.row) as isize);
        } else if cursor.row >= rows.end {
            self.session
                .scroll_history(-((cursor.row - rows.end + 1) as isize));
        }
    }

    fn update_vi_selection(&mut self) {
        let view = self.session.screen().view();
        let range = self.vi.as_ref().unwrap().selection(view);
        match (&mut self.selection, range) {
            (Some(selection), Some(range)) => selection.set_range(view, range),
            (selection, Some(range)) => *selection = Some(Selection::new(view, range, None)),
            (selection, None) => *selection = None,
        }
    }

    pub(in crate::desktop) fn redraw_vi(&mut self) {
        self.painter.invalidate_overlay();
        self.window.request_redraw();
    }

    fn vi_columns(&self) -> usize {
        usize::from(self.session.geometry().pty_size().columns)
    }
}
