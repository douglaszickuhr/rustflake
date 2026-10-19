mod admin_tab;
mod appearance;
mod completion;
mod conn_dialog;
mod db_filter;
mod dbt_store;
mod frame;
mod grid;
mod history_popup;
mod indexer;
mod lang;
mod lineage_view;
mod meta;
mod picker;
mod plan_view;
mod quick_open;
mod schema_tree;
mod settings;
mod sql_tab;
mod table_sections;
mod table_tab;
mod ui;
mod warehouse_tab;
mod workspace;
mod zoom;

use std::time::Instant;

use gpui_kit::*;

rust_i18n::i18n!("locales", fallback = "en");
use rb_core::session::Engine;

gpui_kit::actions!(
    rustflake,
    [
        RunStatement,
        RunScript,
        CancelQuery,
        SaveEdits,
        NewSqlTab,
        CloseTab,
        RefreshNode,
        CopyCells,
        Quit,
        QuickOpen,
        QuickPrev,
        QuickNext,
        TabKey,
        FormatSql,
        ShowHistory,
        ExplainEstimate,
        ExplainAnalyze,
        PlanZoomIn,
        PlanZoomOut,
        PlanFit,
        ToggleComment,
        EditConnection,
        ClearLineage
    ]
);

/// Database runtime shared by the application.
pub struct Db(pub Engine);

impl Global for Db {}

// Icons outside gpui-kit's default set; only these go into the binary.
gpui_kit::assets::icon_assets!(ExtraIcons, [Database, Layers, Table, Sheet, SquareFunction, Columns3, Hash, KeyRound, ListTree, CircleUser, Funnel, Users, RotateCcwClock, Warehouse, SunMoon, Lock]);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        // Logos (simple-icons, CC0) monochrome; the color comes from whoever draws them.
        match path {
            "icons/postgresql.svg" => return Ok(Some(include_bytes!("../assets/icons/postgresql.svg").as_slice().into())),
            "icons/snowflake-logo.svg" => return Ok(Some(include_bytes!("../assets/icons/snowflake.svg").as_slice().into())),
            _ => {}
        }
        match ExtraIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        Ok(paths)
    }
}

/// Dock icon: the binary runs without an .app bundle, so macOS can't find an .icns.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::AnyThread as _;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{MainThreadMarker, NSData};
    let Some(mtm) = MainThreadMarker::new() else { return };
    let data = NSData::with_bytes(include_bytes!("../assets/app-icon.png"));
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        // AppKit marks the setter unsafe; the image is a valid NSImage and we are on the main thread.
        #[allow(unsafe_code)]
        unsafe {
            NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image))
        };
    }
}

fn main() {
    let started = Instant::now();
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    rb_core::config::prepare_app_dir();
    let engine = Engine::new().expect("create database runtime");

    gpui_kit::application().with_assets(AppAssets).run(move |cx: &mut App| {
        gpui_kit::init(cx);
        // Geist (OFL) in the binary: the canvas look doesn't depend on what's installed.
        let fonts: Vec<std::borrow::Cow<'static, [u8]>> = vec![
            include_bytes!("../assets/fonts/Geist-Regular.ttf").as_slice().into(),
            include_bytes!("../assets/fonts/Geist-Medium.ttf").as_slice().into(),
            include_bytes!("../assets/fonts/Geist-SemiBold.ttf").as_slice().into(),
            include_bytes!("../assets/fonts/Geist-Bold.ttf").as_slice().into(),
            include_bytes!("../assets/fonts/GeistMono-Regular.ttf").as_slice().into(),
            include_bytes!("../assets/fonts/GeistMono-Medium.ttf").as_slice().into(),
            include_bytes!("../assets/fonts/GeistMono-Bold.ttf").as_slice().into(),
        ];
        if let Err(e) = cx.text_system().add_fonts(fonts) {
            tracing::warn!("Geist fonts: {e}");
        }
        settings::init(cx);
        lang::init(cx);
        appearance::init(cx);
        #[cfg(target_os = "macos")]
        set_dock_icon();
        cx.set_global(Db(engine));
        cx.bind_keys([
            KeyBinding::new("secondary-enter", RunStatement, Some("SqlTab > Input")),
            KeyBinding::new("secondary-shift-enter", RunScript, Some("SqlTab > Input")),
            // The editor binds nothing to ShowCompletions by default.
            KeyBinding::new("ctrl-space", gpui_kit::component::input::ShowCompletions, Some("SqlTab > Input")),
            // Tab accepts the autocomplete suggestion like Enter; with no menu open, it indents.
            KeyBinding::new("tab", TabKey, Some("SqlTab > Input")),
            // ⌘⇧F formats (like DBeaver); the editor's find and replace, which used ⌘⇧F, moves to ⌘⌥F.
            KeyBinding::new("secondary-shift-f", FormatSql, Some("SqlTab > Input")),
            KeyBinding::new("secondary-/", ToggleComment, Some("SqlTab > Input")),
            KeyBinding::new("secondary-alt-f", gpui_kit::component::input::Replace, Some("SqlTab > Input")),
            KeyBinding::new("secondary-e", ExplainEstimate, Some("SqlTab > Input")),
            KeyBinding::new("secondary-shift-e", ExplainAnalyze, Some("SqlTab > Input")),
            KeyBinding::new("secondary-=", PlanZoomIn, Some("PlanView")),
            KeyBinding::new("secondary-+", PlanZoomIn, Some("PlanView")),
            KeyBinding::new("secondary--", PlanZoomOut, Some("PlanView")),
            KeyBinding::new("secondary-0", PlanFit, Some("PlanView")),
            KeyBinding::new("escape", ClearLineage, Some("PlanView")),
            KeyBinding::new("secondary-=", PlanZoomIn, Some("SqlTab")),
            KeyBinding::new("secondary-+", PlanZoomIn, Some("SqlTab")),
            KeyBinding::new("secondary--", PlanZoomOut, Some("SqlTab")),
            KeyBinding::new("secondary-0", PlanFit, Some("SqlTab")),
            KeyBinding::new("secondary-.", CancelQuery, Some("SqlTab")),
            // The editor binds cmd-. to ToggleCodeActions; the deeper binding wins.
            KeyBinding::new("secondary-.", CancelQuery, Some("SqlTab > Input")),
            KeyBinding::new("secondary-.", CancelQuery, Some("TableTab")),
            KeyBinding::new("secondary-s", SaveEdits, Some("TableTab")),
            KeyBinding::new("secondary-t", NewSqlTab, Some("Workspace")),
            KeyBinding::new("secondary-w", CloseTab, Some("Workspace")),
            KeyBinding::new("f5", RefreshNode, Some("SchemaTree")),
            KeyBinding::new("secondary-r", RefreshNode, Some("SchemaTree")),
            KeyBinding::new("secondary-c", CopyCells, Some("ResultGrid")),
            KeyBinding::new("secondary-q", Quit, None),
            KeyBinding::new("secondary-k", QuickOpen, Some("Workspace")),
            KeyBinding::new("secondary-y", ShowHistory, Some("Workspace")),
            KeyBinding::new("secondary-,", EditConnection, Some("Workspace")),
            // The field uses ↑↓ to move the cursor; the deeper binding wins.
            KeyBinding::new("up", QuickPrev, Some("QuickOpen > Input")),
            KeyBinding::new("down", QuickNext, Some("QuickOpen > Input")),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());

        let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: None,
                appears_transparent: true,
                // Vertical center of the title row (44px + 2px strip).
                traffic_light_position: Some(point(px(16.), px(17.))),
            }),
            ..gpui_kit::component::TitleBar::window_options()
        };
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| workspace::Workspace::new(window, cx))).expect("open window");
        cx.activate(true);
        tracing::info!(elapsed_ms = started.elapsed().as_millis() as u64, "window opened");
    });
}
