//! `ViewRootImpl` — the window-attached root that runs measure → layout →
//! draw into an [`ANativeWindow`] buffer whenever the tree is dirty.
//!
//! Android's version lives on the UI thread between `Choreographer`
//! traversals and the renderer; here the CLI's vsync hook calls
//! [`ViewRootImpl::perform_traversals`] each beat, so an `invalidate()`
//! lands on the next presented frame (design doc §06 "Interaction and
//! Scope").

use crate::widget::Drawable;
use crate::view::{Length, MeasureSpec};
use ovelo_error::Result;
use ovelo_ui::{ANativeWindow, Canvas, Color, FontSet, FrameId, Rect};
use std::rc::Rc;

/// The window's view root.
pub struct ViewRootImpl {
    /// The content view (`Activity.setContentView`), if attached.
    pub content: Option<Box<dyn Drawable>>,
    /// Window width px.
    pub w: i32,
    /// Window height px.
    pub h: i32,
    /// Fonts shared with text widgets.
    pub fonts: Rc<FontSet>,
    /// Root window background.
    pub bg: Color,
}

impl ViewRootImpl {
    /// Empty root at the given size.
    pub fn new(w: i32, h: i32, fonts: Rc<FontSet>) -> Self {
        Self {
            content: None,
            w,
            h,
            fonts,
            bg: Color::srgb(0xFF, 0xFF, 0xFF),
        }
    }

    /// `setContentView` — attach and invalidate.
    pub fn set_content_view(&mut self, v: Box<dyn Drawable>) {
        self.content = Some(v);
    }

    /// Resize (`onWindowAttributesChanged` / config change): forces a full
    /// re-layout next traversal.
    pub fn resize(&mut self, w: i32, h: i32) {
        self.w = w;
        self.h = h;
        if let Some(v) = &mut self.content {
            v.invalidate();
        }
    }

    /// Whether a traversal would draw.
    pub fn is_dirty(&self) -> bool {
        self.content.as_ref().map(|v| v.is_dirty()).unwrap_or(false)
    }

    /// `dispatchTouchEvent`: a press/release at surface px. Click
    /// listeners fire on release (`tap`), matching Android's
    /// `View.performClick` on ACTION_UP inside the view.
    pub fn dispatch_touch(&mut self, down: bool, x: i32, y: i32) -> bool {
        if down {
            return true; // consumed; the tap resolves on release
        }
        self.content
            .as_mut()
            .map(|v| v.dispatch_tap(x, y))
            .unwrap_or(false)
    }

    /// One traversal: when dirty, dequeue a buffer, measure → layout →
    /// draw into it, and post it. `Ok(None)` when nothing changed —
    /// callers skip presentation bookkeeping on None.
    pub fn perform_traversals(&mut self, win: &mut ANativeWindow) -> Result<Option<FrameId>> {
        let Some(v) = &mut self.content else {
            return Ok(None);
        };
        if !v.is_dirty() {
            return Ok(None);
        }
        let surf = win.lock()?;
        {
            let mut c = Canvas::new(surf, &self.fonts);
            c.draw_color(self.bg);
            c.clip_rect(Rect::new(0, 0, self.w, self.h));
            v.measure(
                MeasureSpec::for_child(MeasureSpec::Exactly(self.w), Length::MatchParent),
                MeasureSpec::for_child(MeasureSpec::Exactly(self.h), Length::MatchParent),
            );
            v.layout(0, 0);
            v.draw(&mut c);
            c.restore();
        }
        let id = win.unlock_and_post()?;
        Ok(Some(id))
    }

    /// No-window variant for tests: render into a freestanding surface.
    pub fn draw_to(&mut self, surf: &mut ovelo_ui::Surface) {
        if let Some(v) = &mut self.content {
            let mut c = Canvas::new(surf, &self.fonts);
            c.draw_color(self.bg);
            c.clip_rect(Rect::new(0, 0, self.w, self.h));
            v.measure(
                MeasureSpec::for_child(MeasureSpec::Exactly(self.w), Length::MatchParent),
                MeasureSpec::for_child(MeasureSpec::Exactly(self.h), Length::MatchParent),
            );
            v.layout(0, 0);
            v.draw(&mut c);
            c.restore();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::{Button, TextView};
    use ovelo_ui::Surface;

    fn fonts() -> Rc<FontSet> {
        let mut fs = FontSet::new();
        fs.push(Box::new(
            ovelo_ui::TtfFont::from_bytes(include_bytes!("../tests/geneva.ttf"))
                .expect("font parses"),
        ));
        Rc::new(fs)
    }

    #[test]
    fn dirty_tree_draws_once() {
        let mut root = ViewRootImpl::new(200, 100, fonts());
        assert!(root.content.is_none());
        let mut tv = TextView::new("hi", 16.0, Color::srgb(0, 0, 0), root.fonts.clone());
        tv.view.background = Some(Color::srgb(200, 200, 200));
        root.set_content_view(Box::new(tv));
        assert!(root.is_dirty());
        let mut s = Surface::new(200, 100).unwrap();
        root.draw_to(&mut s);
        assert!(!root.is_dirty());
        // TextView bg is drawn at (0,0) — top-left pixel is the bg gray.
        let (r, g, b, a) = s.pixel(1, 1).straight();
        assert_eq!((r, g, b, a), (200, 200, 200, 255));
    }

    #[test]
    fn button_draws_rounded_bg_then_label() {
        let mut root = ViewRootImpl::new(200, 100, fonts());
        root.set_content_view(Box::new(Button::new(
            "OK",
            16.0,
            Color::srgb(0, 0, 0),
            root.fonts.clone(),
        )));
        let mut s = Surface::new(200, 100).unwrap();
        root.draw_to(&mut s);
        // Button bg covers its measured rect (padding adds ≥24 to a text run).
        let (r, _, _, _) = s.pixel(2, 12).straight();
        assert_eq!(r, 0xDD);
    }

    #[test]
    fn tap_fires_click_listener_and_redraws() {
        use std::cell::Cell;
        let hits = std::rc::Rc::new(Cell::new(0u32));
        let mut root = ViewRootImpl::new(200, 100, fonts());
        let mut ok = Button::new("OK", 16.0, Color::srgb(0, 0, 0), root.fonts.clone());
        let h2 = hits.clone();
        ok.on_click = Some(Box::new(move || h2.set(h2.get() + 1)));
        let mut col = crate::view::ViewGroup::new(
            crate::view::Length::MatchParent,
            crate::view::Length::WrapContent,
        );
        col.children.push(Box::new(ok));
        root.set_content_view(Box::new(col));
        let mut s = Surface::new(200, 100).unwrap();
        root.draw_to(&mut s); // lays out at (0,0), button is ≥24px wide
        assert!(!root.is_dirty());
        // Tap inside the button → click fires, view dirty again.
        assert!(root.dispatch_touch(true, 4, 4));
        assert!(root.dispatch_touch(false, 4, 4));
        assert_eq!(hits.get(), 1);
        assert!(root.is_dirty());
        // Tap outside → nothing.
        assert!(!root.dispatch_touch(false, 199, 99));
        assert_eq!(hits.get(), 1);
    }

    #[test]
    fn traversals_skip_clean_tree() {
        let mut root = ViewRootImpl::new(64, 64, fonts());
        root.set_content_view(Box::new(TextView::new(
            "x",
            12.0,
            Color::srgb(0, 0, 0),
            root.fonts.clone(),
        )));
        let mut win = ANativeWindow::new(
            ovelo_ui::BufferQueue::new(2, 64, 64).expect("queue"),
        );
        assert!(root.perform_traversals(&mut win).unwrap().is_some());
        assert!(root.perform_traversals(&mut win).unwrap().is_none());
    }
}
