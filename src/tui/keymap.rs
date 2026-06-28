//! Key bindings (PRD §11): vim-style primary with arrow-key fallback.
//!
//! Mapping is a pure function of the key event and whether the annotation
//! editor is capturing text, so it can be unit-tested without a terminal.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A semantic action produced by a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    Up,
    Down,
    HalfPageUp,
    HalfPageDown,
    NextChange,
    PrevChange,
    NextCommit,
    PrevCommit,
    ExpandContext,
    CollapseContext,
    FocusToggle,
    /// Switch the diff pane between unified and split layouts.
    ToggleSplit,
    SelectCommit,
    StartSelection,
    Annotate,
    /// Context action of Enter: select a commit (sidebar) or annotate (diff).
    Confirm,
    /// Context action of Space: toggle a visual selection in the diff.
    Space,
    /// Cycle the top band through its views (commits → files → annotations).
    CycleView,
    /// Show a specific band view directly.
    ViewCommits,
    ViewFiles,
    ViewAnnotations,
    Timeline,
    Reopen,
    Edit,
    Delete,
    Undo,
    Cancel,
    EditorChar(char),
    EditorBackspace,
    EditorNewline,
    EditorCycleType,
    EditorSave,
}

/// Map a key to an action. While `editing`, most keys feed the editor's text
/// buffer; otherwise keys drive navigation and commands.
pub fn map(key: KeyEvent, editing: bool) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl && matches!(key.code, KeyCode::Char('c')) {
        return Some(Action::Quit);
    }

    if editing {
        return map_editor(key, ctrl);
    }

    if ctrl {
        return match key.code {
            KeyCode::Char('u') => Some(Action::HalfPageUp),
            KeyCode::Char('d') => Some(Action::HalfPageDown),
            _ => None,
        };
    }

    map_main(key)
}

fn map_editor(key: KeyEvent, ctrl: bool) -> Option<Action> {
    match key.code {
        KeyCode::Esc => Some(Action::Cancel),
        KeyCode::Enter => Some(Action::EditorNewline),
        KeyCode::Backspace => Some(Action::EditorBackspace),
        KeyCode::Char('s') if ctrl => Some(Action::EditorSave),
        KeyCode::Char('t') if ctrl => Some(Action::EditorCycleType),
        KeyCode::Char(c) if !ctrl => Some(Action::EditorChar(c)),
        _ => None,
    }
}

fn map_main(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::Down),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::Up),
        KeyCode::Char('n') => Some(Action::NextChange),
        KeyCode::Char('p') => Some(Action::PrevChange),
        KeyCode::Char('J') => Some(Action::NextCommit),
        KeyCode::Char('K') => Some(Action::PrevCommit),
        KeyCode::Char('+') | KeyCode::Char('=') => Some(Action::ExpandContext),
        KeyCode::Char('-') | KeyCode::Char('_') => Some(Action::CollapseContext),
        KeyCode::Tab => Some(Action::FocusToggle),
        KeyCode::BackTab => Some(Action::CycleView),
        KeyCode::Char('s') => Some(Action::ToggleSplit),
        KeyCode::Char('l') | KeyCode::Right => Some(Action::SelectCommit),
        KeyCode::Enter => Some(Action::Confirm),
        KeyCode::Char(' ') => Some(Action::Space),
        KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc => Some(Action::Cancel),
        KeyCode::Char('v') => Some(Action::StartSelection),
        KeyCode::Char('a') => Some(Action::Annotate),
        KeyCode::Char('t') => Some(Action::Timeline),
        KeyCode::Char('r') => Some(Action::Reopen),
        KeyCode::Char('e') => Some(Action::Edit),
        KeyCode::Char('d') => Some(Action::Delete),
        KeyCode::Char('u') => Some(Action::Undo),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn vim_and_arrows_both_navigate() {
        assert_eq!(map(press(KeyCode::Char('j')), false), Some(Action::Down));
        assert_eq!(map(press(KeyCode::Down), false), Some(Action::Down));
        assert_eq!(map(press(KeyCode::Char('k')), false), Some(Action::Up));
        assert_eq!(map(press(KeyCode::Up), false), Some(Action::Up));
    }

    #[test]
    fn shift_jk_steps_between_commits() {
        assert_eq!(
            map(press(KeyCode::Char('J')), false),
            Some(Action::NextCommit)
        );
        assert_eq!(
            map(press(KeyCode::Char('K')), false),
            Some(Action::PrevCommit)
        );
    }

    #[test]
    fn s_toggles_split_in_main_but_types_in_editor() {
        assert_eq!(
            map(press(KeyCode::Char('s')), false),
            Some(Action::ToggleSplit)
        );
        assert_eq!(
            map(press(KeyCode::Char('s')), true),
            Some(Action::EditorChar('s'))
        );
    }

    #[test]
    fn editor_captures_text_but_honors_ctrl_commands() {
        assert_eq!(
            map(press(KeyCode::Char('x')), true),
            Some(Action::EditorChar('x'))
        );
        assert_eq!(
            map(press(KeyCode::Enter), true),
            Some(Action::EditorNewline)
        );

        let save = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(map(save, true), Some(Action::EditorSave));
    }

    #[test]
    fn ctrl_c_quits_from_any_mode() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(map(ctrl_c, false), Some(Action::Quit));
        assert_eq!(map(ctrl_c, true), Some(Action::Quit));
    }
}
