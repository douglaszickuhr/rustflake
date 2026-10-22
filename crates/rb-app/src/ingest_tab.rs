//! Snowflake pipes (status, recent loads, definition) and stages (location, files, preview).

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::explain::{short_bytes, short_count};
use rb_core::ingest::{self, FileLoad, PipeInfo, PipeStatus, StageFile, StageInfo};
use rb_core::user_view;
use rust_i18n::t;

use crate::meta::ConnTarget;
use crate::schema_tree::OpenTable;
use crate::table_tab::{OpenQuery, spawn_query};
use crate::ui::{Load, Tone};

/// Which object a tree double click opens.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ingest {
    Pipe,
    Stage,
}

/// Opens a pipe's or stage's tab: (kind, schema key `DB.SCHEMA`, name).
pub type OpenIngest = Rc<dyn Fn(Ingest, String, String, &mut Window, &mut App)>;

/// Days of load history shown.
const LOAD_DAYS: u32 = 7;
/// Files listed at most (a bucket can hold millions; a prefix narrows it).
const MAX_FILES: usize = 500;

fn split_key(schema_key: &str) -> (String, String) {
    schema_key.split_once('.').map(|(d, s)| (d.to_string(), s.to_string())).unwrap_or_default()
}

/// `2026-10-21T20:31:14.003Z` (pipe status) or `2026-10-21 20:31:14…` (query results), to Unix seconds.
fn ts(s: &str) -> Option<i64> {
    user_view::parse_ts(&s.replacen('T', " ", 1))
}

fn message(text: String, color: Hsla) -> AnyElement {
    div().p_4().text_sm().text_color(color).child(text).into_any_element()
}

fn header(name: &str, db: &str, schema: &str, badge: Option<Div>, facts: Vec<AnyElement>, notes: Vec<(String, Hsla)>, cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .px_4()
        .py_3()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .child(
            h_flex()
                .gap_3()
                .flex_wrap()
                .items_center()
                .child(div().text_xl().font_family(theme.mono_font_family.clone()).child(name.to_string()))
                .child(div().text_sm().font_family(theme.mono_font_family.clone()).text_color(theme.muted_foreground).child(format!("{db}.{schema}")))
                .children(badge),
        )
        .child(h_flex().gap_4().flex_wrap().items_center().text_sm().text_color(theme.muted_foreground).children(facts))
        .children(notes.into_iter().map(|(n, c)| div().text_sm().text_color(c).whitespace_normal().child(n)))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PipeSection {
    Loads,
    Definition,
}

pub struct PipeTab {
    conn: ConnTarget,
    db: String,
    schema: String,
    name: String,
    info: Load<PipeInfo>,
    status: Load<PipeStatus>,
    /// Waits for the info: the table comes from its `COPY INTO`.
    loads: Load<Vec<FileLoad>>,
    section: PipeSection,
    code: Option<Entity<EditorState>>,
    open_table: OpenTable,
    open_ingest: OpenIngest,
}

impl PipeTab {
    pub fn new(conn: ConnTarget, schema_key: &str, name: String, open_table: OpenTable, open_ingest: OpenIngest, cx: &mut Context<Self>) -> Self {
        let (db, schema) = split_key(schema_key);
        let mut this = PipeTab { conn, db, schema, name, info: Load::Loading, status: Load::Loading, loads: Load::Loading, section: PipeSection::Loads, code: None, open_table, open_ingest };
        this.reload(cx);
        this
    }

    pub fn title(&self) -> SharedString {
        format!("{}.{}.{}", self.db, self.schema, self.name).into()
    }

    pub fn conn_id(&self) -> &str {
        &self.conn.cfg.id
    }

    /// ⌘R and first load: the pipe and its status side by side, then its loads.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        (self.info, self.status, self.loads, self.code) = (Load::Loading, Load::Loading, Load::Loading, None);
        let (d, s, n) = (self.db.clone(), self.schema.clone(), self.name.clone());
        spawn_query(
            self.conn.clone(),
            cx,
            async move |sess| {
                let (names, rows) = sess.query_named(&ingest::pipes_in_schema_sql(&d, &s)).await?;
                Ok(ingest::parse_pipes(&names, &rows).into_iter().find(|p| p.name == n))
            },
            |t: &mut Self, res, cx| {
                t.info = match res {
                    Ok(Some(p)) => {
                        t.load_history(&p, cx);
                        Load::Ready(p)
                    }
                    Ok(None) => Load::Failed(t!("pipe.not_found").to_string()),
                    Err(e) => Load::Failed(e.to_string()),
                };
                if let Load::Failed(e) = &t.info {
                    t.loads = Load::Failed(e.clone());
                }
            },
        );
        let (d, s, n) = (self.db.clone(), self.schema.clone(), self.name.clone());
        spawn_query(
            self.conn.clone(),
            cx,
            async move |sess| {
                let (_, rows) = sess.query_named(&ingest::pipe_status_sql(&d, &s, &n)).await?;
                Ok(rows.into_iter().next().and_then(|r| r.into_iter().next().flatten()))
            },
            |t: &mut Self, res, _| {
                t.status = match res {
                    Ok(json) => json.as_deref().and_then(ingest::parse_pipe_status).map_or_else(|| Load::Failed(String::new()), Load::Ready),
                    Err(e) => Load::Failed(e.to_string()),
                }
            },
        );
        cx.notify();
    }

    fn load_history(&mut self, p: &PipeInfo, cx: &mut Context<Self>) {
        let Some(target) = ingest::pipe_target(p) else {
            self.loads = Load::Failed(t!("pipe.no_target").to_string());
            return;
        };
        let name = p.name.clone();
        spawn_query(
            self.conn.clone(),
            cx,
            async move |sess| {
                let (names, rows) = sess.query_named(&ingest::loads_sql(&target, &name, LOAD_DAYS)).await?;
                Ok(ingest::parse_loads(&names, &rows))
            },
            |t: &mut Self, res, _| t.loads = res.map_or_else(|e| Load::Failed(e.to_string()), Load::Ready),
        );
    }

    fn badge(&self, cx: &App) -> Option<Div> {
        let Load::Ready(st) = &self.status else { return None };
        let tone = match st.state.as_str() {
            _ if st.error.is_some() => Tone::Danger,
            "RUNNING" => Tone::Success,
            s if s.starts_with("STOPPED") || s.contains("FAIL") => Tone::Danger,
            _ => Tone::Neutral,
        };
        Some(crate::ui::badge(tone, cx).child(st.state.to_lowercase().replace('_', " ")))
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut facts: Vec<AnyElement> = Vec::new();
        let mut notes = Vec::new();
        if let Load::Ready(p) = &self.info {
            // `From <stage>` and `Into <table>`, each opening its tab: (label, `DB.SCHEMA.NAME`, open(schema key, name)).
            let open_ingest = self.open_ingest.clone();
            let from: OpenTable = Rc::new(move |s, n, w, cx| open_ingest(Ingest::Stage, s, n, w, cx));
            let links = [("pipe-stage", t!("pipe.from"), ingest::pipe_stage(p), from), ("pipe-target", t!("pipe.into"), ingest::pipe_target(p), self.open_table.clone())];
            for (id, label, fq, open) in links {
                let Some(fq) = fq else { continue };
                let parts: Vec<String> = fq.split('.').map(String::from).collect();
                facts.push(
                    h_flex()
                        .id(id)
                        .gap_1()
                        .child(label.to_string())
                        .child(div().font_family(theme.mono_font_family.clone()).text_color(theme.link).child(fq.clone()))
                        .when(parts.len() == 3, |d| d.cursor_pointer().on_click(move |_, window, cx| open(format!("{}.{}", parts[0], parts[1]), parts[2].clone(), window, cx)))
                        .into_any_element(),
                );
            }
            if p.auto_ingest {
                facts.push(t!("pipe.auto_ingest").to_string().into_any_element());
            }
            facts.extend(p.owner.as_ref().map(|o| format!("{} {o}", t!("tv.owner")).into_any_element()));
            notes.extend(p.invalid_reason.clone().map(|r| (r, theme.danger)));
            notes.extend(p.comment.clone().map(|c| (c, theme.muted_foreground)));
        }
        match &self.status {
            Load::Ready(st) => {
                facts.push(t!("pipe.pending", n = st.pending_files).to_string().into_any_element());
                if let Some(at) = st.last_ingested.as_deref().and_then(ts) {
                    facts.push(t!("pipe.last_ingested", ago = crate::ui::ago(at)).to_string().into_any_element());
                }
                notes.extend(st.error.clone().map(|e| (e, theme.danger)));
            }
            Load::Failed(e) if !e.is_empty() => notes.push((e.clone(), theme.danger)),
            _ => {}
        }
        if let Load::Failed(e) = &self.info {
            notes.push((e.clone(), theme.danger));
        }
        let tab = |id: &'static str, label: String, s: PipeSection, cur: PipeSection, cx: &mut Context<Self>| {
            Button::new(id).label(label).ghost().small().selected(s == cur).on_click(cx.listener(move |t, _, _, cx| {
                t.section = s;
                cx.notify();
            }))
        };
        let cur = self.section;
        let tabs = h_flex().gap_1().child(tab("pipe-loads", t!("pipe.loads").to_string(), PipeSection::Loads, cur, cx)).child(tab(
            "pipe-def",
            t!("task.definition").to_string(),
            PipeSection::Definition,
            cur,
            cx,
        ));
        header(&self.name, &self.db, &self.schema, self.badge(cx), facts, notes, cx).child(tabs)
    }

    fn render_loads(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let loads = match &self.loads {
            Load::Ready(l) if l.is_empty() => return message(t!("pipe.no_loads", days = LOAD_DAYS).to_string(), theme.muted_foreground),
            Load::Ready(l) => l,
            Load::Failed(e) => return message(e.clone(), theme.danger),
            Load::Loading => return message(t!("tree.loading").to_string(), theme.muted_foreground),
        };
        let rows = loads.iter().enumerate().map(|(i, l)| {
            let tone = match l.status.as_str() {
                "Loaded" => Tone::Success,
                "Partially loaded" => Tone::Warning,
                _ => Tone::Danger,
            };
            let at = l.at.as_deref().and_then(ts);
            v_flex()
                .id(("pipe-load", i))
                .px_4()
                .py_2()
                .gap_1()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .text_sm()
                        .child(div().w(px(230.)).flex_shrink_0().whitespace_nowrap().font_family(theme.mono_font_family.clone()).child(at.map(crate::ui::local_time).unwrap_or_default()))
                        .child(div().w(px(130.)).flex_shrink_0().child(crate::ui::badge(tone, cx).child(l.status.to_lowercase())))
                        .child(div().w(px(90.)).flex_shrink_0().text_color(theme.muted_foreground).child(l.rows.map(|r| t!("tv.rows", n = short_count(r as f64)).to_string()).unwrap_or_default()))
                        .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().font_family(theme.mono_font_family.clone()).child(l.file.clone())),
                )
                .children(l.first_error.clone().map(|e| div().text_sm().text_color(theme.danger).whitespace_normal().child(e)))
        });
        div().id("pipe-loads-list").size_full().overflow_y_scroll().children(rows).into_any_element()
    }

    fn render_definition(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Load::Ready(p) = &self.info else { return message(t!("tree.loading").to_string(), theme.muted_foreground) };
        let text = p.definition.clone();
        let code = self.code.get_or_insert_with(|| cx.new(|cx| EditorState::new(window, cx).language("sql").default_value(text))).clone();
        div().size_full().child(Editor::new(&code).h_full()).into_any_element()
    }
}

impl Render for PipeTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.section {
            PipeSection::Loads => self.render_loads(cx),
            PipeSection::Definition => self.render_definition(window, cx),
        };
        v_flex().size_full().child(self.render_header(cx)).child(div().flex_1().min_h_0().child(content))
    }
}

pub struct StageTab {
    conn: ConnTarget,
    db: String,
    schema: String,
    name: String,
    info: Load<StageInfo>,
    /// Newest first, capped, and how many there were.
    files: Load<(Vec<StageFile>, usize)>,
    prefix: Entity<InputState>,
    run_query: OpenQuery,
    _subs: Vec<Subscription>,
}

impl StageTab {
    pub fn new(conn: ConnTarget, schema_key: &str, name: String, run_query: OpenQuery, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (db, schema) = split_key(schema_key);
        let prefix = cx.new(|cx| InputState::new(window, cx).placeholder(t!("stage.prefix").to_string()));
        // LIST can be slow on a big bucket: it runs on Enter (or clearing the field), not per keystroke.
        let subs = vec![cx.subscribe(&prefix, |this: &mut Self, p, ev: &InputEvent, cx| match ev {
            InputEvent::PressEnter { .. } => this.list(cx),
            InputEvent::Change if p.read(cx).value().is_empty() => this.list(cx),
            _ => {}
        })];
        let mut this = StageTab { conn, db, schema, name, info: Load::Loading, files: Load::Loading, prefix, run_query, _subs: subs };
        this.reload(cx);
        this
    }

    pub fn title(&self) -> SharedString {
        format!("{}.{}.{}", self.db, self.schema, self.name).into()
    }

    pub fn conn_id(&self) -> &str {
        &self.conn.cfg.id
    }

    /// ⌘R and first load: the stage, then its files (LIST needs the URL to name them).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        (self.info, self.files) = (Load::Loading, Load::Loading);
        let (d, s, n) = (self.db.clone(), self.schema.clone(), self.name.clone());
        spawn_query(
            self.conn.clone(),
            cx,
            async move |sess| {
                let (names, rows) = sess.query_named(&ingest::stages_in_schema_sql(&d, &s)).await?;
                Ok(ingest::parse_stages(&names, &rows).into_iter().find(|p| p.name == n))
            },
            |t: &mut Self, res, cx| {
                t.info = match res {
                    Ok(Some(s)) => Load::Ready(s),
                    Ok(None) => Load::Failed(t!("stage.not_found").to_string()),
                    Err(e) => Load::Failed(e.to_string()),
                };
                t.list(cx);
            },
        );
        cx.notify();
    }

    fn list(&mut self, cx: &mut Context<Self>) {
        let Load::Ready(stage) = &self.info else {
            self.files = Load::Failed(String::new());
            return;
        };
        let (stage, prefix) = (stage.clone(), self.prefix.read(cx).value().trim().to_string());
        self.files = Load::Loading;
        spawn_query(
            self.conn.clone(),
            cx,
            async move |sess| {
                let (names, rows) = sess.query_named(&ingest::list_sql(&stage, &prefix)).await?;
                Ok(ingest::parse_list(&names, &rows, &stage, MAX_FILES))
            },
            |t: &mut Self, res, _| t.files = res.map_or_else(|e| Load::Failed(e.to_string()), Load::Ready),
        );
        cx.notify();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut facts: Vec<AnyElement> = Vec::new();
        let mut notes = Vec::new();
        let mut badge = None;
        match &self.info {
            Load::Ready(s) => {
                badge = Some(crate::ui::badge(Tone::Neutral, cx).child(s.kind.to_lowercase()));
                facts.extend(s.url.clone().map(|u| div().font_family(theme.mono_font_family.clone()).child(u).into_any_element()));
                let place: Vec<&str> = [s.cloud.as_deref(), s.region.as_deref()].into_iter().flatten().collect();
                if !place.is_empty() {
                    facts.push(place.join(" · ").to_lowercase().into_any_element());
                }
                facts.extend(s.storage_integration.as_ref().map(|i| t!("stage.integration", i = i).to_string().into_any_element()));
                if s.directory {
                    facts.push(t!("stage.directory").to_string().into_any_element());
                }
                facts.extend(s.owner.as_ref().map(|o| format!("{} {o}", t!("tv.owner")).into_any_element()));
                notes.extend(s.comment.clone().map(|c| (c, theme.muted_foreground)));
            }
            Load::Failed(e) => notes.push((e.clone(), theme.danger)),
            Load::Loading => {}
        }
        header(&self.name, &self.db, &self.schema, badge, facts, notes, cx)
    }

    fn render_files(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let (files, total, stage) = match (&self.files, &self.info) {
            (Load::Ready((f, _)), _) if f.is_empty() => return message(t!("stage.no_files").to_string(), theme.muted_foreground),
            (Load::Ready((f, n)), Load::Ready(stage)) => (f, *n, stage),
            (Load::Failed(e), _) => return message(e.clone(), theme.danger),
            _ => return message(t!("tree.loading").to_string(), theme.muted_foreground),
        };
        let rows = files.iter().enumerate().map(|(i, f)| {
            let run = self.run_query.clone();
            let sql = ingest::preview_sql(stage, &f.path);
            h_flex()
                .id(("stage-file", i))
                .px_4()
                .py_1p5()
                .gap_3()
                .items_center()
                .text_sm()
                .border_b_1()
                .border_color(theme.border)
                .child(div().w(px(230.)).flex_shrink_0().whitespace_nowrap().font_family(theme.mono_font_family.clone()).child(f.modified.map(crate::ui::local_time).unwrap_or_default()))
                .child(div().w(px(70.)).flex_shrink_0().text_color(theme.muted_foreground).child(f.size.map(|b| short_bytes(b as f64)).unwrap_or_default()))
                .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().font_family(theme.mono_font_family.clone()).child(f.path.clone()))
                .child(Button::new(("stage-preview", i)).label(t!("stage.preview").to_string()).ghost().xsmall().on_click(move |_, window, cx| run(sql.clone(), window, cx)))
        });
        v_flex()
            .size_full()
            .child(div().px_4().py_1().text_xs().text_color(theme.muted_foreground).child(t!("stage.files", shown = files.len(), total = total).to_string()))
            .child(div().id("stage-files").flex_1().min_h_0().overflow_y_scroll().children(rows))
            .into_any_element()
    }
}

impl Render for StageTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex()
            .size_full()
            .child(self.render_header(cx))
            .child(h_flex().px_3().py_2().border_b_1().border_color(theme.border).child(div().w(px(320.)).child(Input::new(&self.prefix).small().cleanable(true))))
            .child(div().flex_1().min_h_0().child(self.render_files(cx)))
    }
}
