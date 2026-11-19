//! Worker supervision (design P3): guest dispatch runs under
//! `catch_unwind` so a panic in guest code is contained — it cannot take
//! down the host or leak into another app. A crash writes a report to
//! `<app_dir>/data/diagnostics/crash-<n>.txt` and bumps the generation
//! counter; the caller then drops the worker's surface
//! (`Presenter::stop_worker`) so no stale frame displays, and may restart
//! the worker at most once.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

/// Supervises guest dispatch and records crashes on disk.
pub struct Supervisor {
    dir: PathBuf,
    generation: u64,
    restarts: u32,
}

/// What a supervised call produced.
pub enum Outcome<T> {
    /// The call ran to completion.
    Ok(T),
    /// The call panicked; the value is the crash-report path.
    Crashed(PathBuf),
}

impl Supervisor {
    /// Reports go under `dir` (created lazily on first crash).
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            generation: 0,
            restarts: 0,
        }
    }

    /// Generation counter — bumped on every crash; callers tag surfaces
    /// and pending callbacks with it so stale work is dropped.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// True while the worker may still be restarted (once per session).
    pub fn restartable(&self) -> bool {
        self.restarts < 1
    }

    /// Record that a restart was consumed.
    pub fn note_restart(&mut self) {
        self.restarts += 1;
    }

    /// Run `f` catching any panic. On panic a report is written and the
    /// generation bumped; `Crashed(path)` is returned.
    pub fn supervise<T>(&mut self, label: &str, f: impl FnOnce() -> T) -> Outcome<T> {
        match catch_unwind(AssertUnwindSafe(f)) {
            Ok(v) => Outcome::Ok(v),
            Err(payload) => {
                self.generation += 1;
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string payload>".into());
                let path = self.write_report(label, &msg);
                Outcome::Crashed(path)
            }
        }
    }

    fn write_report(&self, label: &str, msg: &str) -> PathBuf {
        let _ = std::fs::create_dir_all(&self.dir);
        let path = self.dir.join(format!("crash-{}.txt", self.generation));
        let body = format!(
            "label={label}\ngeneration={}\npanic={msg}\nrestarts={}\n",
            self.generation, self.restarts
        );
        if std::fs::write(&path, body).is_err() {
            return Path::new("<unwritable>").to_path_buf();
        }
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "ovelo-worker-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[test]
    fn crash_writes_report_and_bumps_generation() {
        let d = dir();
        let mut sup = Supervisor::new(d.clone());
        let out = sup.supervise("launch", || panic!("boom"));
        match out {
            Outcome::Crashed(p) => {
                let body = std::fs::read_to_string(p).unwrap();
                assert!(body.contains("label=launch"));
                assert!(body.contains("panic=boom"));
            }
            Outcome::Ok(_) => panic!("expected crash"),
        }
        assert_eq!(sup.generation(), 1);
        let ok = sup.supervise("again", || 7);
        assert!(matches!(ok, Outcome::Ok(7)));
    }

    #[test]
    fn restart_allowed_once() {
        let mut sup = Supervisor::new(dir());
        assert!(sup.restartable());
        sup.note_restart();
        assert!(!sup.restartable());
    }
}
