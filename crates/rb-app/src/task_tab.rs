//! A Snowflake task: its status and schedule, recent runs, definition and the task graph it belongs to.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::task::{self, DataFlow, TaskInfo, TaskRun};
use rb_core::user_view;
use rust_i18n::t;

use crate::meta::ConnTarget;
use crate::table_tab::spawn_query;
use crate::ui::{Load, Tone};

/// Opens a task's tab: (schema key `DB.SCHEMA`, task name).
pub type OpenTask = Rc<dyn Fn(String, String, &mut Window, &mut App)>;

/// Days of run history shown.
const RUN_DAYS: u32 = 7;
/// Successful runs whose access history makes the data flow (the newest may not be logged yet).
const FLOW_RUNS: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Runs,
    Definition,
    Graph,
}

pub struct TaskTab {
    conn: ConnTarget,
    pub(crate) db: String,
    pub(crate) schema: String,
    pub(crate) name: String,
    info: Load<TaskInfo>,
    runs: Load<Vec<TaskRun>>,
    /// Every task of the database (the graph may cross schemas); loaded when the Graph opens.
    graph: Option<Load<Vec<TaskInfo>>>,
    /// Tables the recent runs read and wrote (`ACCESS_HISTORY`); loaded with the Data flow section.
    flow: Option<Load<DataFlow>>,
    open_table: crate::schema_tree::OpenTable,
    section: Section,
    code: Option<Entity<EditorState>>,
    code_search: Entity<InputState>,
    open_task: OpenTask,
    _subs: Vec<Subscription>,
}

impl TaskTab {
    pub fn new(conn: ConnTarget, schema_key: &str, name: String, open_task: OpenTask, open_table: crate::schema_tree::OpenTable, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (db, schema) = schema_key.split_once('.').map(|(d, s)| (d.to_string(), s.to_string())).unwrap_or_default();
        let code_search = cx.new(|cx| InputState::new(window, cx).placeholder(t!("tv.code_search").to_string()));
        let subs = vec![cx.subscribe_in(&code_search, window, |this, _, ev: &InputEvent, _, cx| match ev {
            InputEvent::Change => this.apply_search(cx),
            InputEvent::PressEnter { shift, .. } => this.step_search(!*shift, cx),
            _ => {}
        })];
        let mut this =
            TaskTab { conn, db, schema, name, info: Load::Loading, runs: Load::Loading, graph: None, flow: None, open_table, section: Section::Runs, code: None, code_search, open_task, _subs: subs };
        this.reload(cx);
        this
    }

    pub fn title(&self) -> SharedString {
        format!("{}.{}.{}", self.db, self.schema, self.name).into()
    }

    pub fn conn_id(&self) -> &str {
        &self.conn.cfg.id
    }

    /// ⌘R and first load: the task and its runs side by side; the graph again if it was open.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let (db, schema, name) = (self.db.clone(), self.schema.clone(), self.name.clone());
        self.info = Load::Loading;
        self.runs = Load::Loading;
        let (d, s, n) = (db.clone(), schema.clone(), name.clone());
        spawn_query(
            self.conn.clone(),
            cx,
            async move |sess| {
                let (names, rows) = sess.query_named(&task::tasks_in_schema_sql(&d, &s)).await?;
                Ok(task::parse_tasks(&names, &rows).into_iter().find(|t| t.name == n))
            },
            |t: &mut Self, res, _| {
                t.code = None;
                t.info = match res {
                    Ok(Some(info)) => Load::Ready(info),
                    Ok(None) => Load::Failed(t!("task.not_found").to_string()),
                    Err(e) => Load::Failed(e.to_string()),
                }
            },
        );
        spawn_query(
            self.conn.clone(),
            cx,
            async move |sess| {
                let (names, rows) = sess.query_named(&task::runs_sql(&db, &schema, &name, RUN_DAYS)).await?;
                Ok(task::parse_runs(&names, &rows))
            },
            |t: &mut Self, res, cx| {
                t.runs = res.map_or_else(|e| Load::Failed(e.to_string()), Load::Ready);
                // The data flow comes from these runs: if its section is open, load it now.
                if t.section == Section::Graph {
                    t.ensure_flow(cx);
                }
            },
        );
        self.flow = None;
        if self.graph.is_some() {
            self.graph = None;
            self.ensure_graph(cx);
        }
        cx.notify();
    }

    fn ensure_graph(&mut self, cx: &mut Context<Self>) {
        if self.graph.is_some() {
            return;
        }
        self.graph = Some(Load::Loading);
        let db = self.db.clone();
        spawn_query(
            self.conn.clone(),
            cx,
            async move |sess| {
                let (names, rows) = sess.query_named(&task::tasks_in_database_sql(&db)).await?;
                Ok(task::parse_tasks(&names, &rows))
            },
            |t: &mut Self, res, _| t.graph = Some(res.map_or_else(|e| Load::Failed(e.to_string()), Load::Ready)),
        );
    }

    /// Tables the last successful runs touched. Needs the runs (their query ids) first.
    fn ensure_flow(&mut self, cx: &mut Context<Self>) {
        let Load::Ready(runs) = &self.runs else { return };
        if self.flow.is_some() {
            return;
        }
        let ok: Vec<&TaskRun> = runs.iter().filter(|r| r.state == "SUCCEEDED" && r.query_id.is_some()).take(FLOW_RUNS).collect();
        let (Some(oldest), false) = (ok.last().and_then(|r| r.started.clone().or(r.scheduled.clone())), ok.is_empty()) else {
            self.flow = Some(Load::Ready(DataFlow::default()));
            return;
        };
        let ids: Vec<String> = ok.iter().filter_map(|r| r.query_id.clone()).collect();
        self.flow = Some(Load::Loading);
        spawn_query(
            self.conn.clone(),
            cx,
            async move |sess| {
                let (names, rows) = sess.query_named(&task::data_flow_sql(&ids, &oldest)).await?;
                Ok(task::parse_data_flow(&names, &rows))
            },
            |t: &mut Self, res, _| t.flow = Some(res.map_or_else(|e| Load::Failed(e.to_string()), Load::Ready)),
        );
    }

    fn go(&mut self, s: Section, cx: &mut Context<Self>) {
        self.section = s;
        if s == Section::Graph {
            self.ensure_graph(cx);
            self.ensure_flow(cx);
        }
        cx.notify();
    }

    fn apply_search(&mut self, cx: &mut Context<Self>) {
        let q = self.code_search.read(cx).value().trim().to_string();
        if let Some(e) = &self.code {
            e.update(cx, |e, cx| {
                if q.is_empty() {
                    e.close_search(cx);
                } else {
                    e.set_search_query(q, true, cx);
                    e.next_search_match(cx);
                }
            });
        }
        cx.notify();
    }

    fn step_search(&mut self, forward: bool, cx: &mut Context<Self>) {
        if let Some(e) = &self.code {
            e.update(cx, |e, cx| if forward { e.next_search_match(cx) } else { e.previous_search_match(cx) });
        }
        cx.notify();
    }

    /// The last run that finished (newest first; scheduled/executing ones are skipped).
    fn last_finished(&self) -> Option<&TaskRun> {
        match &self.runs {
            Load::Ready(r) => r.iter().find(|r| !matches!(r.state.as_str(), "SCHEDULED" | "EXECUTING")),
            _ => None,
        }
    }

    fn status(&self) -> (Tone, String) {
        let Load::Ready(info) = &self.info else { return (Tone::Neutral, t!("tree.loading").to_string()) };
        if info.state != "started" {
            return (Tone::Neutral, t!("task.suspended").to_string());
        }
        match self.last_finished().map(|r| r.state.as_str()) {
            Some("FAILED") => (Tone::Danger, t!("task.last_failed").to_string()),
            _ => (Tone::Success, t!("task.started").to_string()),
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let (tone, status) = self.status();
        let mut facts: Vec<String> = Vec::new();
        if let Load::Ready(i) = &self.info {
            facts.push(task::schedule_label(i));
            facts.push(i.warehouse.clone().map_or_else(|| t!("task.serverless").to_string(), |w| t!("task.warehouse", w = w).to_string()));
            facts.extend(i.owner.as_ref().map(|o| format!("{} {o}", t!("tv.owner"))));
            facts.extend(i.condition.as_ref().map(|c| t!("task.when", c = c).to_string()));
        }
        if let Some(r) = self.last_finished()
            && let Some(at) = r.scheduled.as_deref().and_then(user_view::parse_ts)
        {
            facts.push(t!("task.last_run", ago = crate::ui::ago(at)).to_string());
        }
        let comment = match &self.info {
            Load::Ready(i) => i.comment.clone(),
            Load::Failed(e) => Some(e.clone()),
            Load::Loading => None,
        };
        let tab = |id: &'static str, label: String, s: Section, cur: Section, cx: &mut Context<Self>| {
            Button::new(id).label(label).ghost().small().selected(s == cur).on_click(cx.listener(move |t, _, _, cx| t.go(s, cx)))
        };
        let cur = self.section;
        v_flex()
            .px_4()
            .pt_3()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_3()
                    .flex_wrap()
                    .items_center()
                    .child(div().text_xl().font_family(theme.mono_font_family.clone()).child(self.name.clone()))
                    .child(div().text_sm().font_family(theme.mono_font_family.clone()).text_color(theme.muted_foreground).child(format!("{}.{}", self.db, self.schema)))
                    .child(crate::ui::badge(tone, cx).child(status)),
            )
            .child(h_flex().gap_4().flex_wrap().text_sm().text_color(theme.muted_foreground).children(facts))
            .children(comment.map(|c| div().text_sm().text_color(theme.muted_foreground).child(c)))
            .child(
                h_flex()
                    .gap_1()
                    .pb_2()
                    .child(tab("task-runs", t!("task.runs").to_string(), Section::Runs, cur, cx))
                    .child(tab("task-def", t!("task.definition").to_string(), Section::Definition, cur, cx))
                    .child(tab("task-graph", t!("task.data_flow").to_string(), Section::Graph, cur, cx)),
            )
    }

    fn render_runs(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let runs = match &self.runs {
            Load::Ready(r) if r.is_empty() => return div().p_4().text_color(theme.muted_foreground).child(t!("task.no_runs", days = RUN_DAYS).to_string()).into_any_element(),
            Load::Ready(r) => r,
            Load::Failed(e) => return div().p_4().text_color(theme.danger).child(e.clone()).into_any_element(),
            Load::Loading => return div().p_4().text_color(theme.muted_foreground).child(t!("tree.loading").to_string()).into_any_element(),
        };
        let rows = runs.iter().enumerate().map(|(i, r)| {
            let tone = match r.state.as_str() {
                "SUCCEEDED" => Tone::Success,
                "FAILED" => Tone::Danger,
                "SCHEDULED" | "EXECUTING" => Tone::Info,
                _ => Tone::Neutral,
            };
            let at = r.scheduled.as_deref().and_then(user_view::parse_ts);
            let took = match (r.started.as_deref().and_then(user_view::parse_ts), r.completed.as_deref().and_then(user_view::parse_ts)) {
                (Some(a), Some(b)) if b >= a => crate::admin_tab::human_ms((b - a) as u64 * 1000),
                _ => String::new(),
            };
            v_flex()
                .id(("task-run", i))
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
                        .child(div().w(px(190.)).flex_shrink_0().font_family(theme.mono_font_family.clone()).child(at.map(crate::ui::local_time).unwrap_or_default()))
                        .child(div().w(px(110.)).flex_shrink_0().text_color(theme.muted_foreground).child(at.map(crate::ui::ago).unwrap_or_default()))
                        .child(div().w(px(120.)).flex_shrink_0().child(crate::ui::badge(tone, cx).child(r.state.to_lowercase())))
                        .child(div().w(px(80.)).flex_shrink_0().text_color(theme.muted_foreground).child(took))
                        .child(div().flex_1())
                        .children(r.query_id.clone().map(|q| crate::sql_tab::query_id_button(("task-qid", i), q))),
                )
                .children(r.error.clone().map(|e| div().text_sm().text_color(theme.danger).whitespace_normal().child(e)))
        });
        div().id("task-runs-list").size_full().overflow_y_scroll().children(rows).into_any_element()
    }

    fn render_definition(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Load::Ready(info) = &self.info else { return div().p_4().text_color(theme.muted_foreground).child(t!("tree.loading").to_string()).into_any_element() };
        let code = match &self.code {
            Some(e) => e.clone(),
            None => {
                let text = match &info.condition {
                    Some(c) => format!("-- WHEN {c}\n{}", info.definition),
                    None => info.definition.clone(),
                };
                let e = cx.new(|cx| EditorState::new(window, cx).language("sql").searchable(false).default_value(text));
                self.code = Some(e.clone());
                e
            }
        };
        v_flex()
            .size_full()
            // ⌘F in the code: to the search field (the editor has no find box of its own here).
            .on_action(cx.listener(|t, _: &gpui_kit::component::input::Search, window, cx| t.code_search.update(cx, |s, cx| s.focus(window, cx))))
            .child(
                h_flex()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().w(px(240.)).flex_shrink_0().child(Input::new(&self.code_search).small().cleanable(true)))
                    .when(!self.code_search.read(cx).value().trim().is_empty(), |d| d.child(div().text_xs().text_color(theme.muted_foreground).child(code.read(cx).search_session().matcher.label()))),
            )
            .child(div().flex_1().min_h_0().child(Editor::new(&code).h_full()))
            .into_any_element()
    }

    /// Data flow (what the runs read → the task → what they wrote), plus the task graph when there is one.
    fn render_graph(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = |s: String| div().text_sm().text_color(theme.muted_foreground).child(s);
        let head = |s: String| div().text_xs().text_color(theme.muted_foreground).child(s.to_uppercase());
        let mut body = v_flex().gap_4().p_4();

        // What the task's runs read and wrote.
        body = body.child(head(t!("task.data_flow").to_string()));
        body = body.child(match &self.flow {
            Some(Load::Ready(f)) if f.reads.is_empty() && f.writes.is_empty() => muted(t!("task.no_flow").to_string()).into_any_element(),
            Some(Load::Ready(f)) => {
                let mut id: usize = 0;
                let mut table = |fq: &str| {
                    id += 1;
                    let parts: Vec<&str> = fq.split('.').collect();
                    let open = self.open_table.clone();
                    let target = (parts.len() == 3).then(|| (format!("{}.{}", parts[0], parts[1]), parts[2].to_string()));
                    h_flex()
                        .id(("flow-table", id))
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .child(div().size(px(7.)).flex_shrink_0().rounded_full().bg(theme.primary))
                        .child(
                            v_flex()
                                .child(div().text_sm().font_family(theme.mono_font_family.clone()).child(parts.last().copied().unwrap_or(fq).to_string()))
                                .child(div().text_xs().text_color(theme.muted_foreground).child(parts[..parts.len().saturating_sub(1)].join("."))),
                        )
                        .when_some(target, |d, (schema, name)| d.cursor_pointer().hover(|s| s.bg(theme.secondary_hover)).on_click(move |_, window, cx| open(schema.clone(), name.clone(), window, cx)))
                };
                let reads = v_flex().gap_2().children(f.reads.iter().map(|r| table(r)));
                let writes = v_flex().gap_2().children(f.writes.iter().map(|w| table(w)));
                let me = h_flex().gap_2().px_3().py_2().rounded_md().border_1().border_color(theme.primary).child(div().text_sm().font_family(theme.mono_font_family.clone()).child(self.name.clone()));
                let arrow = || div().text_color(theme.muted_foreground).child("→");
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .flex_none()
                            .items_center()
                            .gap_3()
                            .whitespace_nowrap()
                            .when(!f.reads.is_empty(), |d| d.child(reads).child(arrow()))
                            .child(me)
                            .when(!f.writes.is_empty(), |d| d.child(arrow()).child(writes)),
                    )
                    .child(muted(t!("task.flow_note", n = FLOW_RUNS).to_string()))
                    .into_any_element()
            }
            Some(Load::Failed(e)) => div().text_sm().text_color(theme.danger).child(e.clone()).into_any_element(),
            _ => muted(t!("tree.loading").to_string()).into_any_element(),
        });

        // The task graph (tasks that run before / after this one), only when there is one.
        if let Some(Load::Ready(all)) = &self.graph {
            let focus = format!("{}.{}.{}", self.db, self.schema, self.name);
            let layers = task::graph_layers(all, &focus);
            if layers.iter().map(Vec::len).sum::<usize>() > 1 {
                let mut graph = h_flex().flex_none().items_center().gap_3().whitespace_nowrap();
                let mut id: usize = 0;
                let last = layers.len() - 1;
                for (li, layer) in layers.into_iter().enumerate() {
                    let column = v_flex().gap_2().children(layer.into_iter().map(|t| {
                        id += 1;
                        let here = t.fq() == focus;
                        let open = self.open_task.clone();
                        let (schema_key, name) = (format!("{}.{}", t.db, t.schema), t.name.clone());
                        h_flex()
                            .id(("task-node", id))
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(if here { theme.primary } else { theme.border })
                            .bg(theme.background)
                            .text_sm()
                            .child(div().size(px(7.)).flex_shrink_0().rounded_full().bg(if t.state == "started" { theme.success } else { theme.muted_foreground }))
                            .child(
                                v_flex()
                                    .child(div().font_family(theme.mono_font_family.clone()).child(t.name.clone()))
                                    .child(div().text_xs().text_color(theme.muted_foreground).child(task::schedule_label(&t))),
                            )
                            .when(!here, |d| d.cursor_pointer().hover(|s| s.bg(theme.secondary_hover)).on_click(move |_, window, cx| open(schema_key.clone(), name.clone(), window, cx)))
                    }));
                    graph = graph.child(column);
                    if li < last {
                        graph = graph.child(div().text_color(theme.muted_foreground).child("→"));
                    }
                }
                body = body.child(head(t!("task.graph").to_string())).child(graph);
            }
        }
        div().id("task-graph").size_full().overflow_scroll().child(body).into_any_element()
    }
}

impl Render for TaskTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.section {
            Section::Runs => self.render_runs(cx),
            Section::Definition => self.render_definition(window, cx),
            Section::Graph => self.render_graph(cx),
        };
        v_flex().size_full().child(self.render_header(cx)).child(div().flex_1().min_h_0().child(content))
    }
}
