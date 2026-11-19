//! Android framework surface modeled in Rust (design doc §05, §09).
//!
//! The runtime does not ship AOSP bytecode for framework classes; instead it
//! exposes the same observable semantics — `Context` filesystem conventions,
//! `SharedPreferences` XML documents, the `Looper`/`Handler` message queue,
//! the `Activity` lifecycle, and a minimal `View`/`ViewGroup` measure and
//! layout model — so a driver (the host or the DEX interpreter) can invoke
//! app callbacks with Android's ordering guarantees.

pub mod activity;
pub mod bundle;
pub mod choreographer;
pub mod context;
pub mod looper;
pub mod root;
pub mod prefs;
pub mod resolver;
pub mod sqlite;
pub mod view;
pub mod widget;

pub use activity::{Activity, ActivityError, Lifecycle};
pub use bundle::{BValue, Bundle};
pub use choreographer::{CallbackType, Choreographer, FrameCallback};
pub use context::Context;
pub use looper::{Handler, Looper, Message, MessageQueue};
pub use prefs::{PrefValue, SharedPreferences};
pub use resolver::{ContentProvider, ContentResolver, ContentUri};
pub use root::ViewRootImpl;
pub use sqlite::{Cursor, Database, SqlValue};
pub use view::{Length, MeasureSpec, Size, View, ViewGroup};
pub use widget::{Button, Drawable, TextView};

use ovelo_error::{ErrorCategory, OveloError, Phase};

/// Framework-layer error constructor.
pub(crate) fn err(cause: &str) -> OveloError {
    OveloError::new(ErrorCategory::ServiceMissing, Phase::Service, cause)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_roundtrip_xml() {
        let dir = std::env::temp_dir().join(format!("ovelo-prefs-{}", std::process::id()));
        let path = dir.join("shared_prefs").join("a.xml");
        let mut p = SharedPreferences::open(path.clone());
        p.put("i", PrefValue::Int(42));
        p.put("s", PrefValue::Str("héllo &<>".into()));
        p.put("b", PrefValue::Bool(true));
        p.put("set", PrefValue::StrSet(["x".into(), "y".into()].into()));
        assert!(p.commit().unwrap());
        let xml = std::fs::read_to_string(&path).unwrap();
        assert!(xml.contains("<int name=\"i\" value=\"42\"/>"));

        let p2 = SharedPreferences::open(path);
        assert_eq!(p2.get_int("i", 0), 42);
        assert_eq!(p2.get_string("s", ""), "héllo &<>");
        assert!(p2.get_bool("b", false));
        assert_eq!(p2.get_string_set("set").unwrap().len(), 2);
        assert_eq!(p2.get_int("missing", 7), 7);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn queue_orders_by_time_and_seq() {
        let mut l = Looper::default();
        let h = Handler::new(1);
        h.send_empty_delayed(l.queue(), 2, 50, 100); // due @150
        h.send_empty(l.queue(), 1, 100); // due @100
        h.send_empty_delayed(l.queue(), 3, 50, 100); // due @150, later seq
        assert_eq!(l.loop_once(100).unwrap().what, 1);
        assert!(l.loop_once(120).is_none()); // nothing due until 150
        let msgs = l.drain(150);
        let whats: Vec<i32> = msgs.iter().map(|m| m.what).collect();
        assert_eq!(whats, vec![2, 3]);
    }

    #[test]
    fn lifecycle_callback_order() {
        let mut a = Activity::new();
        a.launch().unwrap();
        assert_eq!(a.pending(), ["onCreate", "onStart", "onResume"]);
        while a.delivered().is_some() {}
        a.pause().unwrap();
        assert_eq!(a.pending(), ["onPause", "onStop"]);
        while a.delivered().is_some() {}
        a.resume().unwrap();
        assert_eq!(a.pending(), ["onRestart", "onStart", "onResume"]);
        while a.delivered().is_some() {}
        a.finish().unwrap();
        assert_eq!(a.pending(), ["onPause", "onStop", "onDestroy"]);
        assert_eq!(a.state(), Lifecycle::Destroyed);
        assert!(a.pause().is_err());
    }

    #[test]
    fn illegal_transition_rejected() {
        let mut a = Activity::new();
        assert!(a.pause().is_err());
        assert!(a.resume().is_err());
    }

    #[test]
    fn linear_group_measure_layout() {
        let mut g = ViewGroup::new(Length::MatchParent, Length::WrapContent);
        g.children.push(Box::new(View::new(
            Length::MatchParent,
            Length::WrapContent,
            Size { w: 10, h: 20 },
        )));
        g.children.push(Box::new(View::new(
            Length::Exact(50),
            Length::Exact(30),
            Size { w: 1, h: 1 },
        )));
        let s = g.measure(MeasureSpec::Exactly(100), MeasureSpec::AtMost(200));
        assert_eq!(s, Size { w: 100, h: 50 });
        g.layout(5, 7);
        assert_eq!(g.children[0].origin(), (5, 7));
        assert_eq!(g.children[1].origin(), (5, 27));
        assert!(g.children[0].measured().w == 100);
    }

    #[test]
    fn context_dirs() {
        let mut c = Context::new("com.example.app", "/data/com.example.app");
        assert_eq!(c.package_name(), "com.example.app");
        assert_eq!(c.files_dir().to_str().unwrap(), "/data/com.example.app/files");
        let p = c.shared_prefs("main");
        assert!(p.lock().unwrap().path().ends_with("shared_prefs/main.xml"));
    }

    #[test]
    fn prefs_shared_across_contexts() {
        // Same file via two Context objects sees one store (AOSP singleton).
        let mut c1 = Context::new("com.example.app", "/data/com.example.app");
        let mut c2 = Context::new("com.example.app", "/data/com.example.app");
        let p1 = c1.shared_prefs("shared-x");
        let p2 = c2.shared_prefs("shared-x");
        p1.lock().unwrap().put("k", PrefValue::Int(9));
        assert_eq!(p2.lock().unwrap().get_int("k", 0), 9);
    }
}
