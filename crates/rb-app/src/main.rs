mod completion;
mod conn_dialog;
mod grid;
mod meta;
mod schema_tree;
mod sql_tab;
mod table_tab;
mod workspace;

use std::time::Instant;

use gpui_kit::*;
use rb_core::session::Engine;

gpui_kit::actions!(rustbeaver, [RunStatement, RunScript, CancelQuery, SaveEdits, NewSqlTab, CloseTab, RefreshNode, CopyCells, Quit]);

/// Runtime de banco compartilhado pela aplicação.
pub struct Db(pub Engine);

impl Global for Db {}

// Ícones fora do conjunto padrão do gpui-kit; só estes entram no binário.
gpui_kit::assets::icon_assets!(ExtraIcons, [Database, Layers, Table, Sheet, SquareFunction, Columns3, Hash, KeyRound, ListTree]);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        // Logos (simple-icons, CC0) monocromáticos; a cor vem de quem desenha.
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

/// Ícone do Dock: o binário roda sem pacote .app, então o macOS não acha um .icns.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::AnyThread as _;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{MainThreadMarker, NSData};
    let Some(mtm) = MainThreadMarker::new() else { return };
    let data = NSData::with_bytes(include_bytes!("../assets/app-icon.png"));
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
    }
}

fn main() {
    let started = Instant::now();
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let engine = Engine::new().expect("criar runtime de banco");

    gpui_kit::application().with_assets(AppAssets).run(move |cx: &mut App| {
        gpui_kit::init(cx);
        #[cfg(target_os = "macos")]
        set_dock_icon();
        cx.set_global(Db(engine));
        cx.bind_keys([
            KeyBinding::new("cmd-enter", RunStatement, Some("SqlTab > Input")),
            KeyBinding::new("cmd-shift-enter", RunScript, Some("SqlTab > Input")),
            // O editor não liga nada a ShowCompletions por padrão.
            KeyBinding::new("ctrl-space", gpui_kit::component::input::ShowCompletions, Some("SqlTab > Input")),
            KeyBinding::new("cmd-.", CancelQuery, Some("SqlTab")),
            // O editor liga cmd-. a ToggleCodeActions; a ligação mais profunda vence.
            KeyBinding::new("cmd-.", CancelQuery, Some("SqlTab > Input")),
            KeyBinding::new("cmd-.", CancelQuery, Some("TableTab")),
            KeyBinding::new("cmd-s", SaveEdits, Some("TableTab")),
            KeyBinding::new("cmd-t", NewSqlTab, Some("Workspace")),
            KeyBinding::new("cmd-w", CloseTab, Some("Workspace")),
            KeyBinding::new("f5", RefreshNode, Some("SchemaTree")),
            KeyBinding::new("cmd-r", RefreshNode, Some("SchemaTree")),
            KeyBinding::new("cmd-c", CopyCells, Some("ResultGrid")),
            KeyBinding::new("cmd-q", Quit, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());

        let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
        let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() };
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| workspace::Workspace::new(window, cx)))
            .expect("abrir janela");
        cx.activate(true);
        tracing::info!(elapsed_ms = started.elapsed().as_millis() as u64, "janela aberta");
    });
}
