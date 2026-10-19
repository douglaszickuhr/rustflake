//! Preferences (`settings.toml`): one copy in memory and one writer, so two quick changes never
//! overwrite each other with a stale read-modify-write.

use std::sync::Mutex;
use std::sync::mpsc;

use gpui_kit::{App, Global};
use rb_core::config::{self, Settings};

pub struct AppSettings {
    current: Settings,
    /// Bumped on every change; the file only ever moves forward.
    version: u64,
    tx: mpsc::Sender<(u64, Settings)>,
}

impl Global for AppSettings {}

/// Version already on disk: the writer thread and `flush` may race, the newest wins.
static WRITTEN: Mutex<u64> = Mutex::new(0);

fn write(version: u64, s: &Settings) {
    let mut written = WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if version <= *written {
        return;
    }
    match config::save_settings(&config::settings_path(), s) {
        Ok(()) => *written = version,
        Err(e) => tracing::warn!("settings.toml: {e}"),
    }
}

/// Loads the file once, at startup, before anything reads a preference.
pub fn init(cx: &mut App) {
    let current = config::load_settings(&config::settings_path());
    let (tx, rx) = mpsc::channel::<(u64, Settings)>();
    std::thread::Builder::new()
        .name("rb-settings".into())
        .spawn(move || {
            while let Ok(mut next) = rx.recv() {
                // A burst of changes: only the last one needs writing.
                while let Ok(newer) = rx.try_recv() {
                    next = newer;
                }
                write(next.0, &next.1);
            }
        })
        .expect("spawn the settings writer");
    cx.set_global(AppSettings { current, version: 0, tx });
}

pub fn get(cx: &App) -> &Settings {
    &cx.global::<AppSettings>().current
}

/// Changes the preferences now; the file is written in the background.
pub fn update(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    let g = cx.global_mut::<AppSettings>();
    f(&mut g.current);
    g.version += 1;
    let _ = g.tx.send((g.version, g.current.clone()));
}

/// Writes the latest version now (on quit the writer thread may not get the chance).
pub fn flush(cx: &App) {
    let g = cx.global::<AppSettings>();
    write(g.version, &g.current);
}
