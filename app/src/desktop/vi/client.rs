use lune_terminal::CellPosition;
use winit::event::KeyEvent;
use winit::keyboard::{Key, NamedKey};

use super::{Command, Mode, normalize};
use crate::desktop::{selection::Selection, shortcuts, tab::Pane};

impl Pane {
    pub(in crate::desktop) fn toggle_vi(&mut self) {
        if self.vi.is_some() {
            self.leave_vi();
            return;
        }
        if self.search.is_some() {
            self.close_search();
        }
        if let Some(button) = self.pressed.take() {
            self.mouse_event(lune_terminal::MouseEvent::Release(button));
        }
        self.clear_selection();
        let screen = self.session.screen();
        let vi = Mode::new(screen.active_buffer(), screen.viewport(), self.vi_columns());
        self.vi = Some(vi);
        log::debug!("Entered Vi mode");
        self.set_ime_allowed();
        self.clear_preedit();
        self.redraw_vi();
    }

    pub(in crate::desktop) fn leave_vi(&mut self) {
        if self.search.is_some() {
            self.close_search();
        }
        self.vi = None;
        log::debug!("Left Vi mode");
        self.set_ime_allowed();
        self.clear_preedit();
        self.follow_output();
        self.redraw_vi();
    }

    pub(in crate::desktop) fn vi_key(&mut self, event: &KeyEvent, copy_pending: &mut bool) {
        let vi = self.vi.as_ref().unwrap();
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
                if *copy_pending || self.selection.is_none() {
                    return;
                }
                self.copy(copy_pending);
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
                self.open_vi_search(direction);
                return;
            }
            Command::RepeatSearch(reverse) => {
                let cursor = vi.cursor;
                if let Some(saved) = &self.search_saved
                    && let Some(found) =
                        saved.find_from(viewport.source(), columns, cursor, reverse)
                {
                    self.vi.as_mut().unwrap().set_cursor(found);
                }
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

    pub(in crate::desktop) fn reveal_vi_cursor(&mut self) {
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

    pub(in crate::desktop) fn update_vi_selection(&mut self) {
        let view = self.session.screen().view();
        let range = self.vi.as_ref().unwrap().selection(view);
        match (&mut self.selection, range) {
            (Some(selection), Some(range)) => selection.set_range(view, range),
            (selection, Some(range)) => *selection = Some(Selection::new(view, range, None)),
            (selection, None) => *selection = None,
        }
    }

    pub(in crate::desktop) fn redraw_vi(&mut self) {
        self.invalidate_overlay();
    }

    fn vi_columns(&self) -> usize {
        usize::from(self.session.geometry().pty_size().columns)
    }
}
