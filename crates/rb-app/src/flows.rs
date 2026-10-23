//! Flows through the real window, headless: what a person does and what they then see.
//! Those that need a database use RB_TEST_DATABASE_URL (like rb-core's) and are skipped without it.
//! HOME points at a temporary folder, so the app's files and ~/.ssh are never the user's.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use gpui_kit::{AnyWindowHandle, App, AppContext as _, Context, Entity, TestAppContext, Window, WindowOptions};
use rb_core::config::{ConfigFile, ConnectionConfig, SslMode};
use rb_core::session::{Credentials, Engine, Session};

use crate::workspace::{TabView, Workspace};

const CONN: &str = "flow";

/// The test database's connection (saved without its password) and the password.
fn target() -> Option<(ConnectionConfig, Option<String>)> {
    let url = std::env::var("RB_TEST_DATABASE_URL").ok()?;
    let pg: tokio_postgres::Config = url.parse().expect("RB_TEST_DATABASE_URL invalid");
    let host = match &pg.get_hosts()[0] {
        tokio_postgres::config::Host::Tcp(h) => h.clone(),
        _ => panic!("RB_TEST_DATABASE_URL needs a TCP host"),
    };
    let cfg = ConnectionConfig {
        environment: None,
        id: CONN.into(),
        name: "Flow tests".into(),
        host,
        port: pg.get_ports().first().copied().unwrap_or(5432),
        database: pg.get_dbname().unwrap_or("postgres").into(),
        user: pg.get_user().unwrap_or("postgres").into(),
        ssl: SslMode::Disable,
        ssh: None,
        snowflake: None,
    };
    Some((cfg, pg.get_password().map(|p| String::from_utf8_lossy(p).into_owned())))
}

/// One temporary HOME per test run, holding the test connection.
fn isolated_home(cfg: &ConnectionConfig) {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        // ponytail: one fixed folder, emptied per run; two test runs at once would share it.
        let home = std::env::temp_dir().join("rustflake-flows");
        let _ = std::fs::remove_dir_all(&home);
        // Set once, before any test reads it; every flow test goes through here first.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("HOME", &home)
        };
        rb_core::config::prepare_app_dir();
        rb_core::config::save(&rb_core::config::connections_path(), &ConfigFile { connections: vec![cfg.clone()] }).expect("save the test connection");
        home
    });
}

struct Flow {
    window: AnyWindowHandle,
    ws: Entity<Workspace>,
    engine: Engine,
    creds: Arc<Credentials>,
    cfg: ConnectionConfig,
}

impl Flow {
    /// The app's window over the test database; None (test skipped) without RB_TEST_DATABASE_URL.
    fn open(cx: &mut TestAppContext) -> Option<Flow> {
        let Some((cfg, password)) = target() else {
            eprintln!("RB_TEST_DATABASE_URL missing; flow test skipped");
            return None;
        };
        isolated_home(&cfg);
        // The database answers on the Engine's threads: real waits, not the scheduler's simulated time.
        cx.executor().allow_parking();
        let engine = Engine::new().expect("runtime");
        let e = engine.clone();
        cx.update(|cx| crate::init(cx, e));
        let (window, ws) = cx.update(|cx| gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| cx.new(|cx| Workspace::new(window, cx))).expect("open the window"));
        cx.update_window(window, |_, window, _| window.activate_window()).expect("window open");
        Some(Flow { window, ws, engine, creds: Arc::new(Credentials { password, ..Default::default() }), cfg })
    }

    fn update<R>(&self, cx: &mut TestAppContext, f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) -> R) -> R {
        cx.update_window(self.window, |_, window, cx| self.ws.update(cx, |ws, cx| f(ws, window, cx))).expect("window open")
    }

    fn read<R>(&self, cx: &mut TestAppContext, f: impl FnOnce(&Workspace, &App) -> R) -> R {
        cx.read(|cx| f(self.ws.read(cx), cx))
    }

    /// Runs what's pending and draws a frame: actions follow the drawn tree, as in the app.
    fn settle(&self, cx: &mut TestAppContext) {
        cx.run_until_parked();
        cx.update_window(self.window, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .expect("window open");
        cx.run_until_parked();
    }

    /// Settles until `done`; the database answers on its own threads, so this waits for real.
    fn wait_until(&self, cx: &mut TestAppContext, what: &str, mut done: impl FnMut(&mut TestAppContext) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            self.settle(cx);
            if done(cx) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            // The app's timers run on the scheduler's clock; real time passes for the database.
            cx.executor().advance_clock(Duration::from_millis(20));
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn connect(&self, cx: &mut TestAppContext) {
        let creds = self.creds.clone();
        self.update(cx, |ws, window, cx| ws.connect_with(CONN, Some(creds), window, cx));
        // Open, with the schema tree's roots in: what a person sees before they start typing.
        self.wait_until(cx, "the connection to open", |cx| self.read(cx, |ws, cx| ws.open.first().is_some_and(|o| !o.meta.read(cx).cache.search_path.is_empty())));
    }

    /// A session of its own, to set up data the way another client would.
    fn session(&self) -> Session {
        pollster::block_on(self.engine.connect(&self.cfg, self.creds.clone())).expect("connect for setup")
    }
}

fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_nanos();
    format!("{prefix}_{n}")
}

#[gpui_kit::test]
fn a_postgres_table_shows_its_rows_when_it_opens(cx: &mut TestAppContext) {
    let Some(flow) = Flow::open(cx) else { return };
    let table = unique("flow_rows");
    pollster::block_on(flow.session().execute(format!("create table public.{table} as select g from generate_series(1, 3) g"))).expect("create the table");
    flow.connect(cx);

    flow.update(cx, |ws, window, cx| ws.open_table(CONN, "public".into(), table.clone(), window, cx));
    flow.wait_until(cx, "the table's rows", |cx| {
        flow.read(cx, |ws, cx| ws.tabs.iter().any(|t| matches!(t, TabView::Table(tab) if tab.read(cx).grid.read(cx).delegate().rs.as_ref().is_some_and(|rs| rs.rows() == 3))))
    });
}

#[gpui_kit::test]
fn cmd_i_lists_the_tables_after_from(cx: &mut TestAppContext) {
    let Some(flow) = Flow::open(cx) else { return };
    let table = unique("flow_complete");
    pollster::block_on(flow.session().execute(format!("create table public.{table} (id int)"))).expect("create the table");
    flow.connect(cx);

    let editor = flow.update(cx, |ws, window, cx| {
        ws.new_sql_tab(window, cx);
        let Some(TabView::Sql(tab)) = ws.tabs.last() else { panic!("no SQL tab") };
        let editor = tab.read(cx).editor.clone();
        editor.update(cx, |e, cx| {
            e.set_value("select * from ", window, cx);
            e.set_cursor_position(lsp_types::Position::new(0, 14), window, cx);
            e.focus(window, cx);
        });
        editor
    });
    flow.settle(cx);
    cx.simulate_keystrokes(flow.window, "cmd-i");
    flow.wait_until(cx, "the completion menu with the table", |cx| {
        cx.read(|cx| {
            let menu = editor.read(cx).completion_menu_state();
            menu.open && menu.items.iter().any(|i| i.label == table)
        })
    });
}

#[gpui_kit::test]
fn cmd_enter_runs_the_statement_and_shows_its_rows(cx: &mut TestAppContext) {
    let Some(flow) = Flow::open(cx) else { return };
    flow.connect(cx);
    let sql = "select g from generate_series(1, 5) g";
    let tab = flow.update(cx, |ws, window, cx| {
        ws.new_sql_tab(window, cx);
        let Some(TabView::Sql(tab)) = ws.tabs.last() else { panic!("no SQL tab") };
        let editor = tab.read(cx).editor.clone();
        editor.update(cx, |e, cx| {
            e.set_value(sql, window, cx);
            e.set_cursor_position(lsp_types::Position::new(0, sql.len() as u32), window, cx);
            e.focus(window, cx);
        });
        tab.clone()
    });
    flow.settle(cx);
    cx.simulate_keystrokes(flow.window, "cmd-enter");
    flow.wait_until(cx, "five rows in the result", |cx| cx.read(|cx| tab.read(cx).result_rows(cx) == [5]));
}

#[gpui_kit::test]
fn a_connection_is_marked_as_connecting_until_it_opens(cx: &mut TestAppContext) {
    let Some(flow) = Flow::open(cx) else { return };
    let creds = flow.creds.clone();
    flow.update(cx, |ws, window, cx| ws.connect_with(CONN, Some(creds), window, cx));
    // The welcome list draws the spinner from this while the attempt runs.
    assert_eq!(flow.read(cx, |ws, _| ws.connecting.clone()).as_deref(), Some(CONN));
    flow.settle(cx);
    flow.wait_until(cx, "the connection to open", |cx| flow.read(cx, |ws, _| !ws.open.is_empty()));
    assert_eq!(flow.read(cx, |ws, _| ws.connecting.clone()), None);
}

#[test]
fn testing_a_connection_reports_the_server_and_saves_nothing() {
    let Some((cfg, password)) = target() else { return eprintln!("RB_TEST_DATABASE_URL missing; flow test skipped") };
    isolated_home(&cfg);
    let before = std::fs::read_to_string(rb_core::config::connections_path()).expect("connections file");
    let engine = Engine::new().expect("runtime");
    let run = |password: String| {
        let creds = crate::conn_dialog::test_creds(&cfg, password, String::new()).expect("credentials");
        crate::conn_dialog::test_line(pollster::block_on(engine.test(&cfg, creds)))
    };

    let ok = run(password.clone().unwrap_or_default()).expect("the test passes");
    // The words follow the UI language; the server and the user don't.
    assert!(ok.contains("PostgreSQL") && ok.contains(&cfg.user), "{ok}");
    let mut wrong = cfg.clone();
    wrong.port = 1;
    let creds = crate::conn_dialog::test_creds(&wrong, String::new(), String::new()).expect("credentials");
    let failed = crate::conn_dialog::test_line(pollster::block_on(engine.test(&wrong, creds))).expect_err("nothing listens on port 1");
    assert!(failed.to_lowercase().contains("refused"), "{failed}");
    assert_eq!(std::fs::read_to_string(rb_core::config::connections_path()).expect("connections file"), before);
}
