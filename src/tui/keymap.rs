//! Key bindings (PRD §11): vim-style primary with arrow-key fallback.
//!
//! Mapping is a pure function of the key event and the context it lands in, so
//! it can be unit-tested without a terminal.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What the keyboard drives right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyContext {
    /// The diff and everything navigable over it: keys drive navigation and
    /// commands.
    Main,
    /// The annotation editor: most keys feed its text buffer.
    Editor,
    /// The key reference: it holds the screen only until the next key.
    Help,
}

/// A semantic action produced by a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Leave the app, or back out of whatever is drawn over the diff.
    Quit,
    /// Leave the app from any context.
    ForceQuit,
    Up,
    Down,
    HalfPageUp,
    HalfPageDown,
    NextChange,
    PrevChange,
    NextAnnotation,
    PrevAnnotation,
    NextCommit,
    PrevCommit,
    ExpandContext,
    CollapseContext,
    /// Switch the diff pane between unified and split layouts.
    ToggleSplit,
    /// Draw or collapse the inline blocks of resolved annotations.
    ToggleResolved,
    StartSelection,
    Annotate,
    /// Context action of Enter: keep a picker's preview, or annotate the line.
    Confirm,
    /// Open a list picker over the diff.
    OpenCommits,
    OpenFiles,
    OpenAnnotations,
    Timeline,
    /// Show the key reference; any key then dismisses it via [`Action::Cancel`].
    ShowHelp,
    Reopen,
    /// Re-read revisions, diff, and the annotation log from disk.
    Reload,
    Edit,
    Delete,
    Undo,
    Cancel,
    EditorChar(char),
    EditorBackspace,
    EditorNewline,
    EditorLeft,
    EditorRight,
    EditorUp,
    EditorDown,
    EditorWordLeft,
    EditorWordRight,
    EditorLineStart,
    EditorLineEnd,
    EditorDeleteForward,
    EditorDeleteWordBack,
    /// Hand the editor body off to `$EDITOR`.
    EditorOpenExternal,
    EditorCycleType,
    EditorSave,
    SpawnAgentForAnnotation,
    SpawnAgentForOpen,
    ToggleAgentLog,
    /// Mark the review finished, releasing every open annotation to an agent
    /// that is waiting on `margin list --watch`.
    HandOff,
}

/// Map a key to an action in `context`.
pub fn map(key: KeyEvent, context: KeyContext) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl && matches!(key.code, KeyCode::Char('c')) {
        return Some(Action::ForceQuit);
    }

    match context {
        // The reference is a glance, not a mode: whatever is pressed next puts
        // the reviewer back in the diff without also acting there.
        KeyContext::Help => Some(Action::Cancel),
        KeyContext::Editor => map_editor(key, ctrl),
        KeyContext::Main if ctrl => match key.code {
            KeyCode::Char('u') => Some(Action::HalfPageUp),
            KeyCode::Char('d') => Some(Action::HalfPageDown),
            _ => None,
        },
        KeyContext::Main => map_main(key),
    }
}

fn map_editor(key: KeyEvent, ctrl: bool) -> Option<Action> {
    match key.code {
        KeyCode::Esc => Some(Action::Cancel),
        KeyCode::Enter => Some(Action::EditorNewline),
        KeyCode::Backspace => Some(Action::EditorBackspace),
        KeyCode::Delete => Some(Action::EditorDeleteForward),
        KeyCode::Left if ctrl => Some(Action::EditorWordLeft),
        KeyCode::Right if ctrl => Some(Action::EditorWordRight),
        KeyCode::Left => Some(Action::EditorLeft),
        KeyCode::Right => Some(Action::EditorRight),
        KeyCode::Up => Some(Action::EditorUp),
        KeyCode::Down => Some(Action::EditorDown),
        KeyCode::Home => Some(Action::EditorLineStart),
        KeyCode::End => Some(Action::EditorLineEnd),
        KeyCode::Char('s') if ctrl => Some(Action::EditorSave),
        KeyCode::Char('t') if ctrl => Some(Action::EditorCycleType),
        KeyCode::Char('w') if ctrl => Some(Action::EditorDeleteWordBack),
        KeyCode::Char('e') if ctrl => Some(Action::EditorOpenExternal),
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
        KeyCode::Char('N') => Some(Action::NextAnnotation),
        KeyCode::Char('P') => Some(Action::PrevAnnotation),
        KeyCode::Char('J') => Some(Action::NextCommit),
        KeyCode::Char('K') => Some(Action::PrevCommit),
        KeyCode::Char('+') | KeyCode::Char('=') => Some(Action::ExpandContext),
        KeyCode::Char('-') | KeyCode::Char('_') => Some(Action::CollapseContext),
        KeyCode::Char('s') => Some(Action::ToggleSplit),
        KeyCode::Char('S') => Some(Action::ToggleResolved),
        KeyCode::Char('c') => Some(Action::OpenCommits),
        KeyCode::Char('f') => Some(Action::OpenFiles),
        KeyCode::Char('A') => Some(Action::OpenAnnotations),
        KeyCode::Enter => Some(Action::Confirm),
        KeyCode::Char(' ') => Some(Action::StartSelection),
        KeyCode::Esc => Some(Action::Cancel),
        KeyCode::Char('v') => Some(Action::StartSelection),
        KeyCode::Char('a') => Some(Action::Annotate),
        KeyCode::Char('t') => Some(Action::Timeline),
        KeyCode::Char('?') => Some(Action::ShowHelp),
        KeyCode::Char('r') => Some(Action::Reopen),
        KeyCode::Char('R') => Some(Action::Reload),
        KeyCode::Char('e') => Some(Action::Edit),
        KeyCode::Char('d') => Some(Action::Delete),
        KeyCode::Char('u') => Some(Action::Undo),
        KeyCode::Char('x') => Some(Action::SpawnAgentForAnnotation),
        KeyCode::Char('X') => Some(Action::SpawnAgentForOpen),
        KeyCode::Char('L') => Some(Action::ToggleAgentLog),
        KeyCode::Char('H') => Some(Action::HandOff),
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
        assert_eq!(
            map(press(KeyCode::Char('j')), KeyContext::Main),
            Some(Action::Down)
        );
        assert_eq!(
            map(press(KeyCode::Down), KeyContext::Main),
            Some(Action::Down)
        );
        assert_eq!(
            map(press(KeyCode::Char('k')), KeyContext::Main),
            Some(Action::Up)
        );
        assert_eq!(map(press(KeyCode::Up), KeyContext::Main), Some(Action::Up));
    }

    #[test]
    fn space_and_v_both_start_selection() {
        assert_eq!(
            map(press(KeyCode::Char(' ')), KeyContext::Main),
            Some(Action::StartSelection)
        );
        assert_eq!(
            map(press(KeyCode::Char('v')), KeyContext::Main),
            Some(Action::StartSelection)
        );
    }

    #[test]
    fn each_list_has_its_own_key_in_main_but_types_in_the_editor() {
        for (key, action) in [
            ('c', Action::OpenCommits),
            ('f', Action::OpenFiles),
            ('A', Action::OpenAnnotations),
        ] {
            assert_eq!(
                map(press(KeyCode::Char(key)), KeyContext::Main),
                Some(action)
            );
            assert_eq!(
                map(press(KeyCode::Char(key)), KeyContext::Editor),
                Some(Action::EditorChar(key))
            );
        }
    }

    #[test]
    fn question_mark_opens_the_key_reference_but_types_in_the_editor() {
        assert_eq!(
            map(press(KeyCode::Char('?')), KeyContext::Main),
            Some(Action::ShowHelp)
        );
        assert_eq!(
            map(press(KeyCode::Char('?')), KeyContext::Editor),
            Some(Action::EditorChar('?'))
        );
    }

    #[test]
    fn shift_np_jumps_between_annotations() {
        assert_eq!(
            map(press(KeyCode::Char('N')), KeyContext::Main),
            Some(Action::NextAnnotation)
        );
        assert_eq!(
            map(press(KeyCode::Char('P')), KeyContext::Main),
            Some(Action::PrevAnnotation)
        );
    }

    #[test]
    fn shift_jk_steps_between_commits() {
        assert_eq!(
            map(press(KeyCode::Char('J')), KeyContext::Main),
            Some(Action::NextCommit)
        );
        assert_eq!(
            map(press(KeyCode::Char('K')), KeyContext::Main),
            Some(Action::PrevCommit)
        );
    }

    #[test]
    fn s_toggles_split_in_main_but_types_in_editor() {
        assert_eq!(
            map(press(KeyCode::Char('s')), KeyContext::Main),
            Some(Action::ToggleSplit)
        );
        assert_eq!(
            map(press(KeyCode::Char('s')), KeyContext::Editor),
            Some(Action::EditorChar('s'))
        );
    }

    #[test]
    fn shift_s_shows_resolved_annotations_in_main_but_types_in_editor() {
        assert_eq!(
            map(press(KeyCode::Char('S')), KeyContext::Main),
            Some(Action::ToggleResolved)
        );
        assert_eq!(
            map(press(KeyCode::Char('S')), KeyContext::Editor),
            Some(Action::EditorChar('S'))
        );
    }

    #[test]
    fn editor_captures_text_but_honors_ctrl_commands() {
        assert_eq!(
            map(press(KeyCode::Char('x')), KeyContext::Editor),
            Some(Action::EditorChar('x'))
        );
        assert_eq!(
            map(press(KeyCode::Enter), KeyContext::Editor),
            Some(Action::EditorNewline)
        );

        let save = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(map(save, KeyContext::Editor), Some(Action::EditorSave));
    }

    #[test]
    fn shift_r_reloads_but_types_in_editor() {
        assert_eq!(
            map(press(KeyCode::Char('R')), KeyContext::Main),
            Some(Action::Reload)
        );
        assert_eq!(
            map(press(KeyCode::Char('R')), KeyContext::Editor),
            Some(Action::EditorChar('R'))
        );
    }

    #[test]
    fn agent_keys_map_in_main_but_type_in_editor() {
        assert_eq!(
            map(press(KeyCode::Char('x')), KeyContext::Main),
            Some(Action::SpawnAgentForAnnotation)
        );
        assert_eq!(
            map(press(KeyCode::Char('X')), KeyContext::Main),
            Some(Action::SpawnAgentForOpen)
        );
        assert_eq!(
            map(press(KeyCode::Char('L')), KeyContext::Main),
            Some(Action::ToggleAgentLog)
        );
        assert_eq!(
            map(press(KeyCode::Char('x')), KeyContext::Editor),
            Some(Action::EditorChar('x'))
        );
    }

    #[test]
    fn shift_h_hands_off_but_types_in_editor() {
        assert_eq!(
            map(press(KeyCode::Char('H')), KeyContext::Main),
            Some(Action::HandOff)
        );
        assert_eq!(
            map(press(KeyCode::Char('H')), KeyContext::Editor),
            Some(Action::EditorChar('H'))
        );
    }

    #[test]
    fn editor_cursor_keys_map_only_while_editing() {
        assert_eq!(
            map(press(KeyCode::Left), KeyContext::Editor),
            Some(Action::EditorLeft)
        );
        assert_eq!(
            map(press(KeyCode::Home), KeyContext::Editor),
            Some(Action::EditorLineStart)
        );
        assert_eq!(
            map(press(KeyCode::Delete), KeyContext::Editor),
            Some(Action::EditorDeleteForward)
        );

        let ctrl_left = KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(
            map(ctrl_left, KeyContext::Editor),
            Some(Action::EditorWordLeft)
        );

        // Outside the editor the same keys do not produce editor actions.
        assert_eq!(map(press(KeyCode::Home), KeyContext::Main), None);
    }

    #[test]
    fn ctrl_e_opens_the_external_editor_while_editing() {
        let ctrl_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(
            map(ctrl_e, KeyContext::Editor),
            Some(Action::EditorOpenExternal)
        );
        assert_eq!(
            map(press(KeyCode::Char('e')), KeyContext::Editor),
            Some(Action::EditorChar('e'))
        );
    }

    #[test]
    fn ctrl_c_quits_from_any_mode() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        for context in [KeyContext::Main, KeyContext::Editor, KeyContext::Help] {
            assert_eq!(map(ctrl_c, context), Some(Action::ForceQuit));
        }
    }

    #[test]
    fn any_key_dismisses_the_key_reference() {
        for code in [
            KeyCode::Char('?'),
            KeyCode::Char('j'),
            KeyCode::Char('d'),
            KeyCode::Char('q'),
            KeyCode::Esc,
            KeyCode::Enter,
            KeyCode::F(5),
        ] {
            assert_eq!(map(press(code), KeyContext::Help), Some(Action::Cancel));
        }
    }
}
