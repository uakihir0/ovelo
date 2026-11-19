//! `Activity` lifecycle: the platform's state machine with the callback
//! sequence a driver must issue, in Android order.
//!
//! The driver (host or interpreter) walks `transition` edges and invokes the
//! returned callback names on the app's Activity instance.

use crate::err;
use ovelo_error::OveloError;

/// Illegal lifecycle transition.
pub type ActivityError = OveloError;

/// Lifecycle states, mirroring `ActivityLifecycleItem` targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    /// Allocated, `onCreate` not yet called.
    PreCreated,
    /// `onCreate` delivered.
    Created,
    /// `onStart` delivered (visible).
    Started,
    /// `onResume` delivered (foreground).
    Resumed,
    /// `onPause` delivered.
    Paused,
    /// `onStop` delivered (not visible).
    Stopped,
    /// `onDestroy` delivered; terminal.
    Destroyed,
}

/// An `Activity` instance's lifecycle plus the callback names in flight.
pub struct Activity {
    state: Lifecycle,
    /// Callbacks the driver must invoke, oldest first.
    pending: Vec<&'static str>,
    /// State surviving process death (`onSaveInstanceState`'s Bundle);
    /// present when the activity is launched with a previous state.
    saved_state: Option<crate::bundle::Bundle>,
}

impl Activity {
    /// A fresh activity, before `onCreate`.
    pub fn new() -> Self {
        Self {
            state: Lifecycle::PreCreated,
            pending: Vec::new(),
            saved_state: None,
        }
    }

    /// Attach a state bundle saved by a previous process instance — set
    /// before `launch()`; `onCreate` then receives it and
    /// `onRestoreInstanceState` is emitted after `onStart`.
    pub fn set_saved_state(&mut self, b: crate::bundle::Bundle) {
        self.saved_state = Some(b);
    }

    /// The bundle passed to `onCreate`, if any.
    pub fn saved_state(&self) -> Option<&crate::bundle::Bundle> {
        self.saved_state.as_ref()
    }

    /// `onSaveInstanceState` — the framework records the callback and the
    /// driver writes the app's state into `out`.
    pub fn save_instance_state(&mut self, out: crate::bundle::Bundle) {
        self.pending.push("onSaveInstanceState");
        self.saved_state = Some(out);
    }

    /// Current lifecycle state.
    pub fn state(&self) -> Lifecycle {
        self.state
    }

    /// Callback names the driver still has to deliver (Android order).
    pub fn pending(&self) -> &[&'static str] {
        &self.pending
    }

    /// Pop the next callback after the driver delivered it.
    pub fn delivered(&mut self) -> Option<&'static str> {
        if self.pending.is_empty() {
            None
        } else {
            Some(self.pending.remove(0))
        }
    }

    /// Launch: PreCreated → Resumed, emitting
    /// `onCreate`/`onStart`/`onResume`.
    pub fn launch(&mut self) -> Result<(), ActivityError> {
        match self.state {
            Lifecycle::PreCreated => {
                if self.saved_state.is_some() {
                    self.pending.extend([
                        "onCreate",
                        "onStart",
                        "onRestoreInstanceState",
                        "onResume",
                    ]);
                } else {
                    self.pending.extend(["onCreate", "onStart", "onResume"]);
                }
                self.state = Lifecycle::Resumed;
                Ok(())
            }
            _ => Err(err("framework.activity.transition")),
        }
    }

    /// Foreground → background: `onPause`/`onStop` (→ Paused → Stopped).
    pub fn pause(&mut self) -> Result<(), ActivityError> {
        match self.state {
            Lifecycle::Resumed => {
                self.pending.extend(["onPause", "onStop"]);
                self.state = Lifecycle::Stopped;
                Ok(())
            }
            _ => Err(err("framework.activity.transition")),
        }
    }

    /// Background → foreground: `onRestart`/`onStart`/`onResume`.
    pub fn resume(&mut self) -> Result<(), ActivityError> {
        match self.state {
            Lifecycle::Stopped | Lifecycle::Paused => {
                self.pending.extend(["onRestart", "onStart", "onResume"]);
                self.state = Lifecycle::Resumed;
                Ok(())
            }
            _ => Err(err("framework.activity.transition")),
        }
    }

    /// Finish: → Destroyed. Emits the missing teardown callbacks from the
    /// current state (`onPause`/`onStop` as needed, then `onDestroy`).
    pub fn finish(&mut self) -> Result<(), ActivityError> {
        match self.state {
            Lifecycle::Destroyed => Err(err("framework.activity.transition")),
            Lifecycle::Resumed => {
                self.pending.extend(["onPause", "onStop", "onDestroy"]);
                self.state = Lifecycle::Destroyed;
                Ok(())
            }
            Lifecycle::Paused | Lifecycle::Stopped => {
                if self.state == Lifecycle::Paused {
                    self.pending.push("onStop");
                }
                self.pending.push("onDestroy");
                self.state = Lifecycle::Destroyed;
                Ok(())
            }
            Lifecycle::Started | Lifecycle::Created => {
                self.pending.extend(["onStop", "onDestroy"]);
                self.state = Lifecycle::Destroyed;
                Ok(())
            }
            Lifecycle::PreCreated => {
                self.pending.push("onDestroy");
                self.state = Lifecycle::Destroyed;
                Ok(())
            }
        }
    }

    /// Configuration-change relaunch: `onPause`/`onStop`/`onDestroy` then
    /// `onCreate`/`onStart`/`onResume` (platform recreate()).
    pub fn recreate(&mut self) -> Result<(), ActivityError> {
        match self.state {
            Lifecycle::Resumed => {
                self.pending.extend([
                    "onPause",
                    "onStop",
                    "onDestroy",
                    "onCreate",
                    "onStart",
                    "onResume",
                ]);
                self.state = Lifecycle::Resumed;
                Ok(())
            }
            _ => Err(err("framework.activity.transition")),
        }
    }
}

impl Default for Activity {
    fn default() -> Self {
        Self::new()
    }
}
