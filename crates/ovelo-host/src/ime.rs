//! IME skeleton (design doc §08 "Japanese IME").
//!
//! On macOS the windowing layer speaks `NSTextInputClient` (composing marked
//! text, commit, selection); `winit` surfaces the same events as
//! `winit::event::Ime::{Preedit, Commit, Enabled, Disabled}`. This module
//! defines the Android-side [`InputConnection`] contract and the pure mapping
//! from host events to [`ImeEvent`]s the guest runtime consumes. The AppKit
//! glue lives in `window` and stays thin.

/// Host IME event delivered to the guest `InputConnection`.
#[derive(Debug, Clone, PartialEq)]
pub enum ImeEvent {
    /// IME session started (field focused).
    Start,
    /// Composing (marked) text changed; `cursor` is the caret byte offset
    /// inside `text`, `None` when the IME hides the caret.
    Composing {
        /// Marked text.
        text: String,
        /// Caret/selection span (byte offsets) within `text`.
        cursor: Option<(usize, usize)>,
    },
    /// Composition committed (final text replaces any composing text).
    Commit {
        /// Committed text.
        text: String,
    },
    /// Composition cancelled without commit (Esc, focus loss).
    Cancel,
    /// IME session ended (field unfocused / IME disabled).
    Finish,
}

/// Android `InputConnection`-shaped sink the guest implements; the host calls
/// these as IME events arrive.
pub trait InputConnection {
    /// Called for every host IME event in order.
    fn on_ime(&mut self, ev: ImeEvent);
    /// Current selection as (anchor, active) char offsets for marked-text
    /// positioning. Default = collapsed caret at 0.
    fn selection(&self) -> (usize, usize) {
        (0, 0)
    }
}

/// Map `winit` IME events to [`ImeEvent`]. Extracted as a pure function so it
/// is unit-testable without a window.
pub fn map_winit(ev: &winit::event::Ime) -> ImeEvent {
    match ev {
        winit::event::Ime::Enabled => ImeEvent::Start,
        winit::event::Ime::Preedit(text, cursor) => ImeEvent::Composing {
            text: text.clone(),
            cursor: *cursor,
        },
        winit::event::Ime::Commit(text) => ImeEvent::Commit { text: text.clone() },
        winit::event::Ime::Disabled => ImeEvent::Finish,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rec(Vec<ImeEvent>);
    impl InputConnection for Rec {
        fn on_ime(&mut self, ev: ImeEvent) {
            self.0.push(ev);
        }
    }

    #[test]
    fn composing_commit_sequence() {
        let mut r = Rec(vec![]);
        for ev in [
            ImeEvent::Start,
            ImeEvent::Composing {
                text: "かな".into(),
                cursor: Some((6, 6)),
            },
            ImeEvent::Commit {
                text: "漢字".into(),
            },
            ImeEvent::Finish,
        ] {
            r.on_ime(ev);
        }
        assert_eq!(r.0.len(), 4);
        assert!(matches!(&r.0[2], ImeEvent::Commit { text } if text == "漢字"));
    }

    #[test]
    fn winit_mapping() {
        use winit::event::Ime;
        assert_eq!(map_winit(&Ime::Enabled), ImeEvent::Start);
        assert_eq!(
            map_winit(&Ime::Preedit("あ".into(), Some((3, 3)))),
            ImeEvent::Composing {
                text: "あ".into(),
                cursor: Some((3, 3))
            }
        );
        assert_eq!(
            map_winit(&Ime::Commit("x".into())),
            ImeEvent::Commit { text: "x".into() }
        );
        assert_eq!(map_winit(&Ime::Disabled), ImeEvent::Finish);
    }
}
