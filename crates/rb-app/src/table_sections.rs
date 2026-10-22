//! Snowflake table view: header and Definition / dbt sections (canvas boards 7, 9 and 10).

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::dbt::{DbtProject, Node, commit_url, file_url};
use rb_core::explain::{short_bytes, short_count};
use rb_core::org_config::{LevelColor, TabKind};
use rb_core::table_info::{self, DefSort, DescSource, LinNode};
use rb_core::user_view;
use rust_i18n::t;

use crate::dbt_store::parsed_ago;
use crate::table_tab::{Section, TableTab, spawn_query};
use crate::ui::Load;

/// Node type color (dbt model / source / seed / outside dbt), same as lineage.
pub(crate) fn kind_dot(kind: &str, theme: &gpui_kit::component::Theme) -> Hsla {
    match kind {
        "model" | "snapshot" => theme.primary,
        "source" => theme.success,
        "seed" => theme.info,
        _ => theme.warning,
    }
}

fn ago(ts: &str) -> Option<String> {
    user_view::parse_ts(ts).map(crate::ui::ago)
}

/// Plain text hover, for `.tooltip(...)`.
/// Lag as people say it: "1 h", "15 min", "40 s".
fn lag_label(secs: u64) -> String {
    match secs {
        s if s >= 3600 && s % 3600 == 0 => format!("{} h", s / 3600),
        s if s >= 60 && s % 60 == 0 => format!("{} min", s / 60),
        s => crate::admin_tab::human_ms(s * 1000),
    }
}

fn tip(text: String) -> impl Fn(&mut Window, &mut App) -> AnyView {
    move |w, cx| Tooltip::new(text.clone()).build(w, cx)
}

impl TableTab {
    /// On Snowflake the schema key is `DB.SCHEMA`.
    pub(crate) fn db_schema(&self) -> (String, String) {
        match self.schema.split_once('.') {
            Some((d, s)) => (d.to_string(), s.to_string()),
            None => (String::new(), self.schema.clone()),
        }
    }

    pub(crate) fn project(&self, cx: &App) -> Option<Arc<DbtProject>> {
        match &self.dbt.as_ref()?.read(cx).project {
            Load::Ready(p) => Some(p.clone()),
            _ => None,
        }
    }

    /// The dbt model/source/seed for this table (exact or, if the manifest is from another target, by name).
    pub(crate) fn node<'a>(&self, p: &'a DbtProject) -> Option<&'a Node> {
        self.node_match(p).map(|(n, _)| n)
    }

    pub(crate) fn node_match<'a>(&self, p: &'a DbtProject) -> Option<(&'a Node, bool)> {
        let (db, schema) = self.db_schema();
        p.manifest.find_table(&db, &schema, &self.table)
    }

    /// Own session (same login): INFORMATION_SCHEMA queries don't compete with the grid.
    fn spawn_query<T: 'static>(
        &mut self,
        cx: &mut Context<Self>,
        run: impl AsyncFnOnce(&rb_core::session::Session) -> Result<T, rb_core::DbError> + 'static,
        done: impl FnOnce(&mut Self, Result<T, rb_core::DbError>, &mut Context<Self>) + 'static,
    ) {
        spawn_query(self.conn.clone(), cx, run, done);
    }

    pub(crate) fn ensure_facts(&mut self, cx: &mut Context<Self>) {
        if self.facts.is_some() || self.meta.read(cx).cache.dialect != rb_core::dialect::Dialect::Snowflake {
            return;
        }
        self.facts = Some(Load::Loading);
        let (db, schema) = self.db_schema();
        let table = self.table.clone();
        self.spawn_query(
            cx,
            async move |s| {
                let (n, r) = s.query_named(&table_info::facts_sql(&db, &schema, &table)).await?;
                Ok(table_info::parse_facts(&n, &r).unwrap_or_default())
            },
            |t, res, cx| {
                if let Ok(f) = &res
                    && (f.iceberg || f.dynamic)
                {
                    t.load_kind_extra(f.dynamic, cx);
                }
                t.facts = Some(res.map_or_else(|e| Load::Failed(e.to_string()), Load::Ready));
            },
        );
    }

    /// Details for the Iceberg / dynamic badge, after the facts say which kind it is.
    fn load_kind_extra(&mut self, dynamic: bool, cx: &mut Context<Self>) {
        self.kind_extra = Some(Load::Loading);
        let (db, schema) = self.db_schema();
        let table = self.table.clone();
        self.spawn_query(
            cx,
            async move |s| {
                if dynamic {
                    let (n, r) = s.query_named(&table_info::dynamic_sql(&db, &schema, &table)).await?;
                    Ok(table_info::parse_dynamic(&n, &r).map(crate::table_tab::KindExtra::Dynamic))
                } else {
                    let (n, r) = s.query_named(&table_info::iceberg_sql(&db, &schema, &table)).await?;
                    Ok(table_info::parse_iceberg(&n, &r, &table).map(crate::table_tab::KindExtra::Iceberg))
                }
            },
            |t, res, _| {
                t.kind_extra = match res {
                    Ok(Some(x)) => Some(Load::Ready(x)),
                    Ok(None) => Some(Load::Failed(t!("tv.kind_no_details").to_string())),
                    Err(e) => Some(Load::Failed(e.to_string())),
                }
            },
        );
    }

    /// "Iceberg" / "Dynamic · lag 1 h" pill next to the name, coloured by the refresh state; details on hover.
    fn kind_badge(&self, cx: &App) -> Option<AnyElement> {
        let Some(Load::Ready(f)) = &self.facts else { return None };
        if !f.iceberg && !f.dynamic {
            return None;
        }
        let extra = self.kind_extra.as_ref();
        let (tone, label, lines): (crate::ui::Tone, String, Vec<String>) = match extra {
            Some(Load::Ready(crate::table_tab::KindExtra::Dynamic(d))) => {
                let latest = d.latest_data.as_deref().and_then(user_view::parse_ts);
                let state = table_info::dynamic_state(d, user_view::now(), latest);
                let (tone, what) = match state {
                    table_info::DynState::OnTime => (crate::ui::Tone::Success, t!("tv.dyn_on_time")),
                    table_info::DynState::Behind => (crate::ui::Tone::Warning, t!("tv.dyn_behind")),
                    table_info::DynState::Failed => (crate::ui::Tone::Danger, t!("tv.dyn_failed")),
                    table_info::DynState::Suspended => (crate::ui::Tone::Neutral, t!("tv.dyn_suspended")),
                };
                let label = match d.target_lag_sec {
                    Some(s) => t!("tv.dyn_lag", lag = lag_label(s)).to_string(),
                    None => t!("tv.dyn_downstream").to_string(),
                };
                let mut lines = vec![what.to_string()];
                lines.extend(d.last_message.clone().filter(|_| state == table_info::DynState::Failed));
                lines.extend(latest.map(|at| t!("tv.dyn_data_as_of", at = crate::ui::local_time(at), ago = crate::ui::ago(at)).to_string()));
                if let (Some(mean), Some(max)) = (d.mean_lag_sec, d.max_lag_sec) {
                    let pct = d.within_target.map(|r| format!("{:.0}", r * 100.)).unwrap_or_else(|| "?".into());
                    lines.push(t!("tv.dyn_lags", mean = lag_label(mean), max = lag_label(max), pct = pct).to_string());
                }
                if !d.recommendations.is_empty() {
                    lines.push(String::new());
                    lines.push(t!("tv.dyn_advice").to_string());
                    lines.extend(d.recommendations.iter().take(3).map(|r| format!("• {r}")));
                }
                (tone, label, lines)
            }
            Some(Load::Ready(crate::table_tab::KindExtra::Iceberg(i))) => {
                let mut lines = Vec::new();
                lines.extend(i.managed.map(|m| if m { t!("tv.ice_managed").to_string() } else { t!("tv.ice_external", catalog = i.catalog.clone().unwrap_or_default()).to_string() }));
                lines.extend(i.catalog.as_ref().filter(|_| i.managed != Some(false)).map(|v| t!("tv.ice_catalog", v = v).to_string()));
                lines.extend(i.external_volume.as_ref().map(|v| t!("tv.ice_volume", v = v).to_string()));
                lines.extend(i.base_location.as_ref().map(|v| t!("tv.ice_location", v = v).to_string()));
                (crate::ui::Tone::Info, t!("tv.iceberg").to_string(), lines)
            }
            Some(Load::Failed(e)) => (crate::ui::Tone::Neutral, if f.dynamic { t!("tv.dynamic") } else { t!("tv.iceberg") }.to_string(), vec![e.clone()]),
            _ => (crate::ui::Tone::Neutral, if f.dynamic { t!("tv.dynamic") } else { t!("tv.iceberg") }.to_string(), vec![t!("tree.loading").to_string()]),
        };
        let text = lines.join("\n");
        Some(div().id("tv-kind-badge").when(!text.is_empty(), |d| d.tooltip(tip(text))).child(crate::ui::badge(tone, cx).child(label)).into_any_element())
    }

    /// ⌘R: fetches again everything already shown (header facts, definition, data, lineage, the open
    /// custom tab). Sections never opened stay unloaded; dbt reads local files and has its own Compile.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.facts = None;
        self.kind_extra = None;
        self.ensure_facts(cx);
        if self.def_raw.take().is_some() {
            self.col_lineage.clear();
            self.ensure_definition(cx);
        }
        if self.data_started {
            self.load(cx);
        }
        if let Some(l) = &self.lineage {
            l.update(cx, |l, cx| l.reload(cx));
        }
        if let Section::Custom(i) = self.section
            && self.custom.get(i).is_some_and(|c| c.opened)
        {
            self.load_custom(i, cx);
        }
        cx.notify();
    }

    /// The dbt code's search field changed: highlight the matches and go to the first.
    pub(crate) fn apply_code_search(&mut self, cx: &mut Context<Self>) {
        let q = self.code_search.read(cx).value().trim().to_string();
        if let Some((e, _)) = &self.sql_view {
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

    pub(crate) fn step_code_search(&mut self, forward: bool, cx: &mut Context<Self>) {
        if let Some((e, _)) = &self.sql_view {
            e.update(cx, |e, cx| if forward { e.next_search_match(cx) } else { e.previous_search_match(cx) });
        }
        cx.notify();
    }

    /// Columns first (the list shows as soon as they arrive); tags and masking policies then load
    /// in parallel, each on its own session, and fill in the classification when they land.
    pub(crate) fn ensure_definition(&mut self, cx: &mut Context<Self>) {
        if self.def_raw.is_some() {
            return;
        }
        self.def_raw = Some(Load::Loading);
        let (db, schema) = self.db_schema();
        let table = self.table.clone();
        let (d, sc, tb) = (db.clone(), schema.clone(), table.clone());
        self.spawn_query(
            cx,
            async move |s| {
                let t0 = std::time::Instant::now();
                let (n, r) = s.query_named(&table_info::columns_sql(&d, &sc, &tb)).await?;
                tracing::info!("definition {tb}: columns in {:?}", t0.elapsed());
                Ok(table_info::parse_columns(&n, &r))
            },
            move |t, res, cx| match res {
                Ok(cols) => {
                    let raw = crate::table_tab::DefRaw { cols, class_tags: HashMap::new(), masking: HashMap::new(), other_tags: HashMap::new(), tag_error: None, pending: 2 };
                    t.def_raw = Some(Load::Ready(raw));
                    t.load_def_extras(db, schema, table, cx);
                }
                Err(e) => t.def_raw = Some(Load::Failed(e.to_string())),
            },
        );
    }

    /// Tags and policies are extras: without permission the columns stay, and the summary says
    /// classification didn't load (instead of looking like "nothing classified").
    fn load_def_extras(&mut self, db: String, schema: String, table: String, cx: &mut Context<Self>) {
        fn landed(t: &mut TableTab, apply: impl FnOnce(&mut crate::table_tab::DefRaw)) {
            if let Some(Load::Ready(raw)) = &mut t.def_raw {
                apply(raw);
                raw.pending = raw.pending.saturating_sub(1);
            }
        }
        let org = self.conn.org.clone();
        let (d, sc, tb) = (db.clone(), schema.clone(), table.clone());
        self.spawn_query(
            cx,
            async move |s| {
                let t0 = std::time::Instant::now();
                let out = s.query_named(&table_info::tags_sql(&d, &sc, &tb)).await;
                tracing::info!("definition {tb}: tags in {:?}", t0.elapsed());
                out
            },
            move |t, res, _| {
                landed(t, |raw| match res {
                    Ok((n, r)) => {
                        raw.class_tags = table_info::parse_class_tags(&org, &n, &r);
                        raw.other_tags = table_info::parse_all_tags(&n, &r);
                    }
                    Err(e) => raw.tag_error = Some(e.to_string()),
                })
            },
        );
        self.spawn_query(
            cx,
            async move |s| {
                let t0 = std::time::Instant::now();
                let out = s.query_named(&table_info::policies_sql(&db, &schema, &table)).await;
                tracing::info!("definition {table}: policies in {:?}", t0.elapsed());
                out
            },
            move |t, res, _| {
                landed(t, |raw| match res {
                    Ok((n, r)) => raw.masking = table_info::parse_masking(&n, &r),
                    Err(e) => {
                        raw.tag_error.get_or_insert(e.to_string());
                    }
                })
            },
        );
    }

    fn select_col(&mut self, col: String, cx: &mut Context<Self>) {
        // Clicking the selected row again closes the panel.
        if self.selected_col.as_deref() == Some(col.as_str()) {
            self.selected_col = None;
            cx.notify();
            return;
        }
        self.selected_col = Some(col.clone());
        self.ensure_col_lineage(col, cx);
        cx.notify();
    }

    /// Starts loading where `col` comes from (once; the panel and the hover card share it).
    pub(crate) fn ensure_col_lineage(&mut self, col: String, cx: &mut Context<Self>) {
        if !self.col_lineage.contains_key(&col) {
            self.col_lineage.insert(col.clone(), Load::Loading);
            let (db, schema) = self.db_schema();
            let table = self.table.clone();
            let c2 = col.clone();
            self.spawn_query(
                cx,
                async move |s| {
                    let (n, r) = s.query_named(&table_info::column_lineage_sql(&db, &schema, &table, &c2)).await?;
                    Ok(table_info::parse_column_lineage(&n, &r))
                },
                move |t, res, _| {
                    t.col_lineage.insert(col, res.map_or_else(|e| Load::Failed(e.to_string()), Load::Ready));
                },
            );
            cx.notify();
        }
    }

    fn go(&mut self, s: Section, cx: &mut Context<Self>) {
        self.section = s;
        if s == Section::Definition {
            self.ensure_definition(cx);
        }
        if s == Section::Lineage && self.lineage.is_none() {
            let (db, schema) = self.db_schema();
            // The kind lets the focus show its DDL (views) before any lineage row arrives.
            let kind = match &self.facts {
                Some(Load::Ready(f)) if !f.kind.is_empty() && f.kind != "BASE TABLE" => Some(f.kind.clone()),
                _ => None,
            };
            let focus = rb_core::lineage::Obj { key: rb_core::lineage::ObjKey::table(&db, &schema, &self.table), kind, masked: false };
            let (conn, dbt, open_table, open_query) = (self.conn.clone(), self.dbt.clone(), self.open_table.clone(), self.open_query.clone());
            self.lineage = Some(cx.new(|cx| crate::lineage_view::LineageView::new(focus, conn, dbt, open_table, open_query, cx)));
        }
        if s == Section::Data && !self.data_started && self.target.is_some() {
            self.load(cx);
        }
        // Custom tab: runs/opens only the first time; after that, ↻ or the button.
        if let Section::Custom(i) = s
            && let Some(c) = self.custom.get_mut(i)
            && !c.opened
        {
            c.opened = true;
            match c.kind.clone() {
                TabKind::Sql(_) => self.load_custom(i, cx),
                TabKind::Url(u) => cx.open_url(&u),
            }
        }
        cx.notify();
    }

    /// Name, dbt badge, owner, tags, Query, facts and the section tabs.
    pub(crate) fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let (db, schema) = self.db_schema();
        let project = self.project(cx);
        let node = project.as_deref().and_then(|p| self.node_match(p)).map(|(n, _)| n.clone());
        // Same qualifier as the grid: quotes for lowercase/spaced names.
        let fq = rb_core::dialect::Dialect::Snowflake.qualified(&self.schema, &self.table);
        let open_query = self.open_query.clone();
        let mut top = h_flex().gap_3().flex_wrap().items_center().child(div().text_xl().font_family(theme.mono_font_family.clone()).child(self.table.clone())).child({
            // Click copies the full name (the one to paste in SQL).
            let full = format!("{db}.{schema}.{}", self.table);
            div()
                .id("tv-fqn")
                .text_sm()
                .font_family(theme.mono_font_family.clone())
                .text_color(theme.muted_foreground)
                .cursor_pointer()
                .hover(|d| d.text_color(theme.foreground))
                .tooltip(tip(t!("tv.copy_name", name = full.clone()).to_string()))
                .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(full.clone())))
                .child(format!("{db}.{schema}"))
        });
        top = top.children(self.kind_badge(cx));
        if let Some(n) = &node {
            top = top.when_some(n.owner.clone(), |d, o| d.child(div().text_sm().text_color(theme.muted_foreground).child(format!("{} {o}", t!("tv.owner"))))).children(
                n.tags.iter().enumerate().map(|(i, tg)| div().id(("tv-dbt-tag", i)).tooltip(tip(t!("tv.dbt_tag").to_string())).child(crate::ui::badge(crate::ui::Tone::Neutral, cx).child(tg.clone()))),
            );
        }
        let facts = match &self.facts {
            Some(Load::Ready(f)) => {
                let kind = match f.kind.as_str() {
                    "BASE TABLE" => t!("tv.kind_table").to_string(),
                    "VIEW" => t!("tv.kind_view").to_string(),
                    "MATERIALIZED VIEW" => t!("tv.kind_matview").to_string(),
                    other => other.to_lowercase(),
                };
                // Short on screen; the exact value on hover.
                let at = |ts: &Option<String>| ts.as_deref().and_then(user_view::parse_ts);
                let mut v = vec![(kind, at(&f.created).map(|s| t!("tv.created_at", at = crate::ui::local_time(s)).to_string()))];
                v.extend(f.rows.map(|r| (t!("tv.rows", n = short_count(r as f64)).to_string(), Some(t!("tv.rows", n = crate::ui::grouped(r)).to_string()))));
                v.extend(f.bytes.map(|b| (short_bytes(b as f64), Some(t!("tv.bytes_exact", n = crate::ui::grouped(b)).to_string()))));
                v.extend(at(&f.last_altered).map(|s| (t!("tv.changed", ago = crate::ui::ago(s)).to_string(), Some(crate::ui::local_time(s)))));
                v
            }
            Some(Load::Failed(m)) => vec![(m.clone(), None)],
            _ => vec![(t!("tree.loading").to_string(), None)],
        };
        let facts = facts.into_iter().enumerate().map(|(i, (text, hover))| div().id(("tv-fact", i)).when_some(hover, |d, h| d.tooltip(tip(h))).child(text));
        let tab = |id: &'static str, label: String, s: Section, cur: Section, cx: &mut Context<Self>| {
            Button::new(id).label(label).ghost().small().selected(s == cur).on_click(cx.listener(move |t, _, _, cx| t.go(s, cx)))
        };
        // Long comment: own line, wrapping, up to 3 lines (the facts line doesn't wrap text).
        let comment = match &self.facts {
            Some(Load::Ready(f)) => f.comment.as_deref().map(str::trim).filter(|c| !c.is_empty()).map(String::from),
            _ => None,
        };
        let cur = self.section;
        v_flex()
            .px_4()
            .pt_3()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(top)
            .child(h_flex().gap_4().flex_wrap().text_sm().text_color(theme.muted_foreground).children(facts))
            .when_some(comment, |d, c| {
                // Comment can be a whole YAML: closed, a single line; open, a scrollable box.
                let open = self.comment_open;
                let flat = c.split_whitespace().collect::<Vec<_>>().join(" ");
                let long = flat.chars().count() > 220 || c.contains('\n');
                let short = if flat.chars().count() > 220 { format!("{}…", flat.chars().take(220).collect::<String>()) } else { flat };
                d.child(
                    h_flex()
                        .gap_2()
                        .items_start()
                        .max_w(px(960.))
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(if open {
                            div().id("tv-comment").flex_1().max_h(px(200.)).overflow_y_scroll().whitespace_normal().child(c).into_any_element()
                        } else {
                            div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(short).into_any_element()
                        })
                        .when(long, |d| {
                            d.child(Button::new("tv-comment-more").label(if open { t!("tv.less") } else { t!("tv.more") }.to_string()).ghost().xsmall().on_click(cx.listener(|t, _, _, cx| {
                                t.comment_open = !t.comment_open;
                                cx.notify();
                            })))
                        }),
                )
            })
            .child(
                h_flex()
                    .gap_1()
                    .pb_1()
                    .child(tab("tv-def", t!("tv.definition").to_string(), Section::Definition, cur, cx))
                    .child(tab("tv-data", t!("tv.data").to_string(), Section::Data, cur, cx))
                    .child(tab("tv-dbt", "dbt".into(), Section::Dbt, cur, cx))
                    .child(tab("tv-lineage", t!("tv.lineage").to_string(), Section::Lineage, cur, cx))
                    .children(self.custom.iter().enumerate().map(|(i, c)| {
                        let s = Section::Custom(i);
                        Button::new(("tv-custom", i)).label(c.name.clone()).ghost().small().selected(s == cur).on_click(cx.listener(move |t, _, _, cx| t.go(s, cx)))
                    }))
                    // Action at the end of the sections row: in the title row it wrapped onto a line of its own.
                    .child(div().flex_1())
                    .child(Button::new("tv-query").label(t!("tv.query").to_string()).outline().small().on_click(move |_, w, cx| open_query(format!("select * from {fq} limit 100;"), w, cx))),
            )
    }

    /// Board 7: columns with description (dbt or comment), classification, masking and tests.
    pub(crate) fn render_definition(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let crate::table_tab::DefRaw { cols, class_tags: tags, masking: masks, other_tags: others, tag_error: tag_err, pending } = match &self.def_raw {
            Some(Load::Ready(r)) => r.clone(),
            Some(Load::Failed(m)) => return div().p_4().text_color(theme.danger).child(m.clone()).into_any_element(),
            _ => return div().p_4().text_color(theme.muted_foreground).child(t!("tree.loading").to_string()).into_any_element(),
        };
        // Without a classification tag in the config, "N classified" would be false.
        let has_class = self.conn.org.has_classification();
        let project = self.project(cx);
        let node = project.as_deref().and_then(|p| self.node(p));
        let dbt_cols = node.map(|n| n.columns.clone()).unwrap_or_default();
        let mut tests_by_col: HashMap<String, Vec<String>> = HashMap::new();
        if let (Some(p), Some(n)) = (project.as_deref(), node) {
            for t in p.manifest.tests_of(&n.unique_id) {
                if let Some(c) = &t.column {
                    tests_by_col.entry(c.clone()).or_default().push(t.kind.clone());
                }
            }
        }
        let rows = table_info::merge(&cols, &dbt_cols, &tags, &masks, &tests_by_col);
        let sm = table_info::summary(&rows);
        let query = self.def_search.read(cx).value().to_string();
        let (sort, asc) = self.def_sort;
        let pairs = rows.iter().map(|r| (r.clone(), others.iter().find(|(k, _)| k.eq_ignore_ascii_case(&r.col.name)).map(|(_, v)| v.clone()).unwrap_or_default())).collect();
        let shown = table_info::def_view(pairs, &query, sort, asc);
        let mono = theme.mono_font_family.clone();
        // Empty columns give their width to the description (it was being squeezed to nothing).
        let (has_policy, has_tests) = (rows.iter().any(|r| r.masked_by.is_some()), rows.iter().any(|r| !r.tests.is_empty()));
        // No description anywhere: no empty column; tags and policy take the width instead of being cut.
        let has_desc = rows.iter().any(|r| r.desc.is_some());
        let tag_value_w = if has_desc { 90. } else { 240. };
        let policy_w = if has_desc { 170. } else { 280. };
        // Narrower than this, the table scrolls sideways instead of squeezing the description.
        let widths = [44., 200., 130., if has_desc { 260. } else { 0. }, 220.].into_iter().chain(has_class.then_some(64.)).chain(has_policy.then_some(policy_w)).chain(has_tests.then_some(140.));
        let (n, sum) = widths.fold((0, 0.), |(n, s), w| (n + 1, s + w));
        let min_w = sum + 12. * (n - 1) as f32 + 24.;
        let mut table = v_flex().min_w(px(min_w)).child(
            h_flex()
                .px_3()
                .py_2()
                .gap_3()
                .text_xs()
                .text_color(theme.muted_foreground)
                .border_b_1()
                .border_color(theme.border)
                .child(self.sort_head(DefSort::Position, "#".into(), cx).w(px(44.)).flex_shrink_0().justify_end())
                .child(self.sort_head(DefSort::Name, t!("tv.col_column").to_string(), cx).w(px(200.)).flex_shrink_0())
                .child(self.sort_head(DefSort::Type, t!("tv.col_type").to_string(), cx).w(px(130.)).flex_shrink_0())
                .when(has_desc, |d| d.child(self.sort_head(DefSort::Description, t!("tv.col_desc").to_string(), cx).flex_1().min_w(px(200.))))
                .when(has_class, |d| d.child(div().w(px(64.)).flex_shrink_0().child(t!("tv.col_class").to_string())))
                .child(self.sort_head(DefSort::Tags, t!("tv.tags").to_string(), cx).w(px(220.)).flex_shrink_0().when(!has_desc, |d| d.flex_1()))
                .when(has_policy, |d| d.child(self.sort_head(DefSort::Policy, t!("tv.col_policy").to_string(), cx).w(px(policy_w)).flex_shrink_0()))
                .when(has_tests, |d| d.child(div().w(px(140.)).flex_shrink_0().child(t!("tv.col_tests").to_string()))),
        );
        for (i, (r, all_tags)) in shown.iter().enumerate() {
            let name = r.col.name.clone();
            let selected = self.selected_col.as_deref() == Some(name.as_str());
            // Text that wraps inside the cell; the source goes at the end of the same text, in gray.
            let desc = match (&r.desc, r.source) {
                (Some(d), src) => {
                    let tag = if src == DescSource::Dbt { t!("tv.src_dbt") } else { t!("tv.src_comment") }.to_string();
                    let text = format!("{d}  {tag}");
                    let muted = HighlightStyle { color: Some(theme.muted_foreground), ..Default::default() };
                    div().w_full().whitespace_normal().child(StyledText::new(text.clone()).with_highlights([(d.len()..text.len(), muted)]))
                }
                // Empty: the summary already counts the undocumented ones.
                (None, _) => div(),
            };
            // Like Snowsight: first tag (by name) as a "⌂ NAME | VALUE" pill and "+N"; hover lists all.
            let col_tags: Vec<_> = all_tags.iter().filter(|t| t.on_column()).cloned().collect();
            let tags_cell = match col_tags.first() {
                Some(first) => {
                    let pill = || h_flex().h(px(22.)).px_2().gap_1().rounded_full().border_1().border_color(theme.border).text_xs();
                    let tip = col_tags.clone();
                    h_flex()
                        .id(("def-tags", i))
                        .gap_1()
                        .min_w_0()
                        .tooltip(move |w, cx| {
                            let tip = tip.clone();
                            Tooltip::element(move |_, cx| tag_cards(&tip, cx)).build(w, cx)
                        })
                        .child(
                            pill()
                                .min_w_0()
                                .child(Icon::new(IconName::Tag).xsmall().text_color(theme.muted_foreground))
                                .child(div().min_w_0().overflow_hidden().text_ellipsis().text_color(theme.muted_foreground).child(first.name.clone()))
                                .child(div().h(px(12.)).w(px(1.)).bg(theme.border))
                                .child(div().flex_shrink_0().max_w(px(tag_value_w)).overflow_hidden().text_ellipsis().font_weight(FontWeight::SEMIBOLD).child(first.value.clone())),
                        )
                        .when(col_tags.len() > 1, |d| d.child(pill().flex_shrink_0().text_color(theme.muted_foreground).child(format!("+{}", col_tags.len() - 1))))
                        .into_any_element()
                }
                None => div().into_any_element(),
            };
            // Policy name only (the panel shows DB.SCHEMA.NAME).
            let policy = r.masked_by.as_deref().map(|p| p.rsplit('.').next().unwrap_or(p).to_string());
            table = table.child(
                h_flex()
                    .id(("def-row", i))
                    .px_3()
                    .py_2()
                    .gap_3()
                    .items_start()
                    .text_sm()
                    .border_b_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme.secondary))
                    .hover(|d| d.bg(theme.secondary_hover))
                    .child(div().w(px(44.)).flex_shrink_0().text_right().font_family(mono.clone()).text_color(theme.muted_foreground).child(r.col.position.to_string()))
                    .child({
                        // Hover the name: where the column comes from, drawn (stays open to click a source).
                        let (tab, col) = (cx.entity(), name.clone());
                        div()
                            .id(("def-name", i))
                            .w(px(200.))
                            .flex_shrink_0()
                            .font_family(mono.clone())
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(name.clone())
                            .hoverable_tooltip(move |_, cx| cx.new(|cx| crate::col_lineage_card::ColLineageCard::new(tab.clone(), col.clone(), cx)).into())
                    })
                    .child(div().w(px(130.)).flex_shrink_0().font_family(mono.clone()).text_color(theme.muted_foreground).overflow_hidden().text_ellipsis().child(r.col.kind.clone()))
                    .when(has_desc, |d| d.child(div().flex_1().min_w(px(200.)).overflow_hidden().child(desc)))
                    .when(has_class, |d| d.child(div().w(px(64.)).flex_shrink_0().child(class_badge(r.level, r.masked_by.is_some(), &self.conn.org, cx))))
                    .child(div().w(px(220.)).flex_shrink_0().when(!has_desc, |d| d.flex_1()).child(tags_cell))
                    .when(has_policy, |d| d.child(div().w(px(policy_w)).flex_shrink_0().text_xs().font_family(mono.clone()).overflow_hidden().text_ellipsis().children(policy)))
                    .when(has_tests, |d| d.child(div().w(px(140.)).flex_shrink_0().child(tests_cell(i, &r.tests, cx))))
                    .on_click(cx.listener(move |t, _, _, cx| t.select_col(name.clone(), cx))),
            );
        }
        let summary = h_flex()
            .gap_2()
            .px_3()
            .pb_2()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(t!("tv.sum_columns", n = sm.columns).to_string())
            // Until tags land, "0 classified" would be wrong: say they're loading instead.
            .when(has_class && pending == 0, |d| d.child("·").child(t!("tv.sum_classified", n = sm.classified).to_string()))
            .when(pending > 0, |d| d.child("·").child(t!("tv.tags_loading").to_string()))
            .child("·")
            .child(t!("tv.sum_tests", n = sm.tests).to_string())
            .when(sm.undocumented > 0, |d| d.child("·").child(div().text_color(theme.warning).child(t!("tv.sum_undocumented", n = sm.undocumented).to_string())))
            .when_some(tag_err, |d, e| d.child("·").child(div().text_color(theme.danger).child(t!("tv.class_unavailable", error = e).to_string())))
            .when(!query.trim().is_empty(), |d| d.child("·").child(t!("tv.def_matches", n = shown.len()).to_string()));
        let search = div().px_3().pb_2().max_w(px(420.)).child(Input::new(&self.def_search).small().cleanable(true));
        let selected = self.selected_col.clone().and_then(|c| rows.iter().find(|r| r.col.name == c).cloned());
        let aside = selected.map(|r| {
            let extra = others.iter().find(|(k, _)| k.eq_ignore_ascii_case(&r.col.name)).map(|(_, v)| v.clone()).unwrap_or_default();
            self.render_column_panel(&r, &extra, project.as_deref(), cx)
        });
        h_flex()
            .size_full()
            .items_start()
            .child(v_flex().id("def-main").flex_1().min_w_0().h_full().overflow_scroll().p_3().child(search).child(summary).child(table))
            .when_some(aside, |d, a| {
                let close = cx.listener(|t, _, _, cx| {
                    t.selected_col = None;
                    cx.notify();
                });
                d.child(crate::frame::side_panel("def-aside", 360., close, a, cx))
            })
            .into_any_element()
    }

    /// Header cell that sorts by its column; clicking it again flips the direction.
    fn sort_head(&self, by: DefSort, label: String, cx: &mut Context<Self>) -> Stateful<Div> {
        let (cur, asc) = self.def_sort;
        let arrow = (cur == by).then_some(if asc { "↑" } else { "↓" });
        h_flex()
            .id(SharedString::from(format!("def-sort-{by:?}")))
            .gap_1()
            .cursor_pointer()
            .hover(|d| d.text_color(cx.theme().foreground))
            .when(cur == by, |d| d.text_color(cx.theme().foreground))
            .child(label)
            .children(arrow)
            .on_click(cx.listener(move |t, _, _, cx| {
                t.def_sort = if t.def_sort.0 == by { (by, !t.def_sort.1) } else { (by, true) };
                cx.notify();
            }))
    }

    fn render_column_panel(&mut self, r: &table_info::DefRow, extra_tags: &[table_info::ColTag], project: Option<&DbtProject>, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let head = |s: String| div().pt_2().text_xs().text_color(theme.muted_foreground).child(s.to_uppercase());
        let nullable = if r.col.nullable { t!("tv.nullable") } else { t!("tv.not_null") }.to_string();
        let default = r.col.default.clone().map_or_else(|| t!("tv.no_default").to_string(), |d| t!("tv.default", d = d).to_string());
        let mut out = v_flex()
            .gap_2()
            .child(div().text_base().font_family(theme.mono_font_family.clone()).child(r.col.name.clone()))
            .child(div().text_xs().text_color(theme.muted_foreground).child(format!("{} · {nullable} · {default}", r.col.kind)))
            .child(head(t!("tv.col_desc").to_string()))
            .child(div().text_sm().child(r.desc.clone().unwrap_or_else(|| t!("tv.no_desc").to_string())));
        if r.source == DescSource::Dbt
            && let Some(c) = &r.col.comment
        {
            out = out.child(div().text_xs().text_color(theme.muted_foreground).child(t!("tv.sf_comment", c = c).to_string()));
        }
        if r.level.is_some() || r.masked_by.is_some() {
            out = out.child(head(t!("tv.protection").to_string()));
            if let Some(l) = r.level {
                let label = self.conn.org.level_label(l).map_or(format!("C{l}"), |n| format!("C{l} · {n}"));
                out = out.child(h_flex().gap_2().text_sm().child(label).when_some(r.tag.clone(), |d, tg| d.child(div().text_xs().text_color(theme.muted_foreground).child(tg))));
            }
            if let Some(p) = &r.masked_by {
                out = out.child(h_flex().gap_1().text_sm().child(Icon::new(IconName::Lock).xsmall()).child(t!("tv.masked_by", p = p).to_string()));
            }
        }
        if !extra_tags.is_empty() {
            out = out.child(head(t!("tv.tags").to_string()));
            for tg in extra_tags {
                out = out.child(
                    h_flex().gap_2().text_xs().child(div().font_family(theme.mono_font_family.clone()).child(tg.name.clone())).child(div().text_color(theme.muted_foreground).child(tg.value.clone())),
                );
            }
        }
        out = out.child(head(t!("tv.comes_from").to_string()));
        match self.col_lineage.get(&r.col.name) {
            Some(Load::Ready(l)) if l.is_empty() => out = out.child(div().text_sm().text_color(theme.muted_foreground).child(t!("tv.no_lineage").to_string())),
            Some(Load::Ready(l)) => {
                for n in l {
                    out = out.child(lineage_step(n, project, &theme));
                }
                out = out.child(div().text_xs().text_color(theme.muted_foreground).child(t!("tv.lineage_lag").to_string()));
            }
            Some(Load::Failed(m)) => out = out.child(div().text_xs().text_color(theme.danger).child(format!("{m}\n{}", t!("tv.hint_lineage")))),
            _ => out = out.child(div().text_sm().text_color(theme.muted_foreground).child(t!("tree.loading").to_string())),
        }
        if !r.tests.is_empty() {
            out = out.child(head(t!("tv.col_tests").to_string()));
            let node = project.and_then(|p| self.node(p));
            for kind in &r.tests {
                let status = project.zip(node).and_then(|(p, n)| {
                    let t = p.manifest.tests_of(&n.unique_id).into_iter().find(|t| &t.kind == kind && t.column.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(&r.col.name)))?;
                    p.results.get(&t.unique_id).cloned()
                });
                out = out.child(h_flex().gap_2().text_sm().child(crate::ui::badge(crate::ui::Tone::Info, cx).child(kind.clone())).when_some(status, |d, (s, when)| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(match s.as_str() {
                                "pass" => theme.success,
                                "warn" => theme.warning,
                                "skipped" => theme.muted_foreground,
                                _ => theme.danger,
                            })
                            .child(format!("{s} · {}", ago(&when.replace('T', " ")).unwrap_or_default())),
                    )
                }));
            }
        }
        out
    }

    /// `dbt compile --select <model>` with live output; when done, shows the compiled SQL.
    fn compile(&mut self, node: Node, cx: &mut Context<Self>) {
        // Without dbt set on this connection the click did nothing at all; now it says why.
        let Some(cfg) = self.dbt.as_ref().and_then(|d| d.read(cx).cfg.clone()) else {
            self.compiled = Some((node.unique_id.clone(), Load::Failed(t!("dbt.not_configured").to_string())));
            cx.notify();
            return;
        };
        if self.compiling {
            return;
        }
        self.compiling = true;
        self.compile_live.clear();
        cx.notify();
        let log = rb_core::dbt::log_path();
        let (log2, uid) = (log.clone(), node.unique_id.clone());
        let task = cx.background_spawn(async move { rb_core::dbt::compile_model(&cfg, &node, &log2) });
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                let tail = rb_core::dbt::log_tail(&log, 8);
                let go = this
                    .update(cx, |t, cx| {
                        if !tail.is_empty() {
                            t.compile_live = tail;
                            cx.notify();
                        }
                        t.compiling
                    })
                    .unwrap_or(false);
                if !go {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let res = task.await;
            this.update(cx, |t, cx| {
                t.compiling = false;
                t.compile_live.clear();
                match res {
                    Ok(sql) => {
                        t.compiled = Some((uid, Load::Ready((sql, Some(std::time::SystemTime::now())))));
                        t.show_compiled = true;
                        t.show_blame = false;
                    }
                    Err(e) => t.compiled = Some((uid, Load::Failed(e.to_string()))),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_blame(&mut self, dir: std::path::PathBuf, file: String, cx: &mut Context<Self>) {
        self.show_blame = !self.show_blame;
        if self.show_blame && self.blame.as_ref().is_none_or(|(f, l)| *f != file || matches!(l, Load::Failed(_))) {
            self.blame = Some((file.clone(), Load::Loading));
            let task = cx.background_spawn(async move { rb_core::dbt::blame(&dir, &file) });
            cx.spawn(async move |this, cx| {
                let res = task.await;
                this.update(cx, |t, cx| {
                    if let Some((_, l)) = t.blame.as_mut() {
                        *l = res.map_or_else(|e| Load::Failed(e.to_string()), Load::Ready);
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    /// Code with who last touched each chunk; clicking the commit opens it on GitHub.
    fn render_blame(&mut self, project: &DbtProject, file: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let lines = match &self.blame {
            Some((f, Load::Ready(l))) if f == file => l.clone(),
            Some((f, Load::Failed(m))) if f == file => return div().p_4().text_color(theme.danger).child(m.clone()).into_any_element(),
            _ => return div().p_4().text_color(theme.muted_foreground).child(t!("tree.loading").to_string()).into_any_element(),
        };
        let git = project.git.clone();
        let mono = theme.mono_font_family.clone();
        uniform_list("blame", lines.len(), move |range, _, cx| {
            let theme = cx.theme().clone();
            range
                .map(|i| {
                    let l = &lines[i];
                    // Only the first line of each chunk shows the commit; the rest stay clean.
                    let first = i == 0 || lines[i - 1].sha != l.sha;
                    let who = if l.uncommitted { t!("tv.uncommitted").to_string() } else { format!("{} · {}", l.author, crate::ui::ago(l.time)) };
                    let url = commit_url(&git, &l.sha);
                    let summary = l.summary.clone();
                    h_flex()
                        .h(px(20.))
                        .text_xs()
                        .child(
                            div()
                                .id(("blame-c", i))
                                .w(px(260.))
                                .flex_shrink_0()
                                .px_2()
                                .h_full()
                                .border_r_1()
                                .border_color(theme.border)
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_color(theme.muted_foreground)
                                .when(first, |d| d.child(who))
                                .when(first && url.is_some(), |d| {
                                    let u = url.clone().unwrap_or_default();
                                    d.cursor_pointer().hover(|d| d.text_color(theme.primary)).on_click(move |_, _, cx| cx.open_url(&u))
                                })
                                .when(first, |d| d.tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(summary.clone()).build(w, cx))),
                        )
                        .child(div().w(px(40.)).flex_shrink_0().text_right().pr_2().text_color(theme.muted_foreground).child((i + 1).to_string()))
                        .child(div().flex_1().min_w_0().font_family(mono.clone()).whitespace_nowrap().child(l.text.replace('\t', "    ")))
                })
                .collect()
        })
        .size_full()
        .into_any_element()
    }

    /// Board 9: model facts on the left, SQL (source/compiled) on the right.
    pub(crate) fn render_dbt(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(store) = self.dbt.clone() else { return div().into_any_element() };
        let project = match &store.read(cx).project {
            Load::Ready(p) => p.clone(),
            _ => {
                // Not configured / no manifest / loading: the board 10 state card.
                return v_flex()
                    .p_4()
                    .gap_3()
                    .max_w(px(640.))
                    .child(div().text_base().child(t!("tv.dbt_missing").to_string()))
                    .child(store.clone())
                    .when(store.read(cx).cfg.is_none(), |d| d.child(div().text_sm().text_color(theme.muted_foreground).child(t!("tv.dbt_setup_hint").to_string())))
                    .into_any_element();
            }
        };
        let Some(node) = self.node(&project).cloned() else {
            return div().p_4().text_color(theme.muted_foreground).child(t!("tv.not_a_model").to_string()).into_any_element();
        };
        let tests = project.manifest.tests_of(&node.unique_id);
        let mut by_kind: Vec<(String, Vec<String>)> = Vec::new();
        for t in &tests {
            let what = t
                .detail
                .clone()
                .map(|d| if t.kind == "accepted_values" { t!("tv.n_values", n = d).to_string() } else { d })
                .or_else(|| t.column.clone())
                .unwrap_or_else(|| t!("tv.model_level").to_string());
            match by_kind.iter_mut().find(|(k, _)| *k == t.kind) {
                Some((_, v)) => v.push(what),
                None => by_kind.push((t.kind.clone(), vec![what])),
            }
        }
        let head = |s: String| div().pt_2().text_xs().text_color(theme.muted_foreground).child(s.to_uppercase());
        let row = |k: String, v: String| {
            h_flex()
                .gap_3()
                .py_1()
                .text_sm()
                .border_b_1()
                .border_color(theme.border)
                .child(div().w(px(110.)).flex_shrink_0().text_color(theme.muted_foreground).child(k))
                .child(div().min_w_0().child(v))
        };
        let mut left =
            v_flex().gap_2().child(head(t!("tv.col_desc").to_string())).child(div().text_sm().child(if node.description.is_empty() { t!("tv.no_desc").to_string() } else { node.description.clone() }));
        let mat = [node.materialized.clone(), node.strategy.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
        left = left
            .when(!mat.is_empty(), |d| d.child(row(t!("tv.materialized").to_string(), mat)))
            .when(!node.unique_key.is_empty(), |d| d.child(row(t!("tv.unique_key").to_string(), node.unique_key.join(", "))))
            .when_some(node.owner.clone(), |d, o| d.child(row(t!("tv.owner").to_string(), o)))
            .when(node.meta.iter().any(|(k, _)| k != "owner"), |d| {
                // One line per key (already flattened: `atlan.attributes.ownerUsers`).
                let mut list = v_flex().gap_0p5().min_w_0();
                for (k, v) in node.meta.iter().filter(|(k, _)| k != "owner") {
                    list = list
                        .child(v_flex().child(div().text_xs().font_family(theme.mono_font_family.clone()).text_color(theme.muted_foreground).child(k.clone())).child(div().text_sm().child(v.clone())));
                }
                d.child(
                    h_flex()
                        .gap_3()
                        .py_1()
                        .items_start()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(div().w(px(110.)).flex_shrink_0().text_sm().text_color(theme.muted_foreground).child(t!("tv.meta").to_string()))
                        .child(list),
                )
            })
            .when(!node.tags.is_empty(), |d| {
                d.child(
                    h_flex()
                        .gap_1()
                        .py_1()
                        .child(div().w(px(110.)).text_sm().text_color(theme.muted_foreground).child(t!("tv.tags").to_string()))
                        .children(node.tags.iter().map(|t| crate::ui::badge(crate::ui::Tone::Neutral, cx).child(t.clone()))),
                )
            });
        left = left.child(head(t!("tv.tests_n", n = tests.len()).to_string()));
        for (kind, cols) in by_kind {
            left = left.child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .text_sm()
                    .child(crate::ui::badge(crate::ui::Tone::Info, cx).child(kind))
                    .child(div().min_w_0().text_xs().font_family(theme.mono_font_family.clone()).text_color(theme.muted_foreground).child(cols.join(", "))),
            );
        }
        let refresh_store = store.clone();
        left = left.child(div().flex_1()).child(
            h_flex()
                .gap_2()
                .pt_3()
                .border_t_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(t!("tv.manifest_footer", target = project.target, ago = parsed_ago(&project)).to_string())
                .child(div().flex_1())
                .child(
                    Button::new("tv-refresh")
                        .label(t!("dbt.refresh").to_string())
                        .outline()
                        .xsmall()
                        .loading(store.read(cx).refreshing)
                        .on_click(move |_, _, cx| refresh_store.update(cx, |s, cx| s.refresh(cx))),
                ),
        );

        // Read-only SQL; the text switches between source and compiled.
        // Compiled: the file in target/compiled (previous compile/run), else the manifest's.
        if self.compiled.as_ref().is_none_or(|(u, _)| *u != node.unique_id) {
            let found = rb_core::dbt::compiled_on_disk(&project.dir, &node).map(|(sql, t)| (sql, Some(t))).or_else(|| node.compiled_code.clone().map(|c| (c, None)));
            self.compiled = found.map(|f| (node.unique_id.clone(), Load::Ready(f)));
        }
        let compiled_sql = match &self.compiled {
            Some((u, Load::Ready((sql, when)))) if *u == node.unique_id => Some((sql.clone(), *when)),
            _ => None,
        };
        let compiled = self.show_compiled && compiled_sql.is_some();
        let code = if compiled { compiled_sql.clone().map(|c| c.0).unwrap_or_default() } else { node.raw_code.clone() };
        let key = format!("{}:{compiled}:{}", node.unique_id, code.len());
        let editor = match &self.sql_view {
            Some((e, k)) if *k == key => e.clone(),
            _ => {
                // No floating find box: the search field at the top left drives it (⌘F focuses that field).
                let e = cx.new(|cx| EditorState::new(window, cx).language("sql").searchable(false).default_value(code));
                let q = self.code_search.read(cx).value().trim().to_string();
                if !q.is_empty() {
                    e.update(cx, |e, cx| e.set_search_query(q, true, cx));
                }
                self.sql_view = Some((e.clone(), key));
                e
            }
        };
        let url = file_url(&project.git, &node.file);
        let local = project.dir.join(&node.file);
        let has_compiled = compiled_sql.is_some();
        let compile_node = node.clone();
        let toggle = |id: &'static str, label: String, on: bool, value: bool, cx: &mut Context<Self>| {
            Button::new(id).label(label).xsmall().ghost().selected(on).on_click(cx.listener(move |t, _, _, cx| {
                t.show_compiled = value;
                t.show_blame = false;
                cx.notify();
            }))
        };
        // Sources (and anything else without SQL): no editor, Compile or Source/Compiled; the details
        // are the content, with the file path and the links to it.
        if node.raw_code.trim().is_empty() {
            let open_local = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let local = local.clone();
                cx.background_spawn(async move {
                    let _ = std::process::Command::new("open").arg(&local).status();
                })
                .detach();
            };
            return v_flex()
                .size_full()
                .child(
                    h_flex()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .font_family(theme.mono_font_family.clone())
                                .text_color(theme.muted_foreground)
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(node.file.clone()),
                        )
                        .when_some(url, |d, u| d.child(Button::new("tv-gh").label(t!("tv.open_github").to_string()).outline().xsmall().on_click(move |_, _, cx| cx.open_url(&u))))
                        .child(Button::new("tv-ed").label(t!("tv.open_editor").to_string()).outline().xsmall().on_click(open_local)),
                )
                .child(div().id("dbt-source").flex_1().min_h_0().overflow_y_scroll().p_4().child(div().max_w(px(720.)).child(left)))
                .child(div().px_3().py_1().text_xs().text_color(theme.muted_foreground).child(format!("{} @ {}", project.git.branch, project.git.sha)))
                .into_any_element();
        }
        let right = v_flex()
            // ⌘F inside the code (the editor passes it up, having no find box): to the search field.
            .on_action(cx.listener(|t, _: &gpui_kit::component::input::Search, window, cx| t.code_search.update(cx, |s, cx| s.focus(window, cx))))
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                h_flex()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    // Search first, at the top left like the other tabs; the count of matches next to it.
                    .child(div().w(px(240.)).flex_shrink_0().child(Input::new(&self.code_search).small().cleanable(true)))
                    .when(!self.code_search.read(cx).value().trim().is_empty(), |d| {
                        d.child(div().flex_shrink_0().text_xs().text_color(theme.muted_foreground).child(editor.read(cx).search_session().matcher.label()))
                    })
                    .child(div().flex_1().min_w_0().text_xs().font_family(theme.mono_font_family.clone()).text_color(theme.muted_foreground).overflow_hidden().text_ellipsis().child(node.file.clone()))
                    // How to look at the file: one segmented control (Source | Compiled | Blame)…
                    .child({
                        let blame_on = self.show_blame && !compiled;
                        let (dir, file) = (project.dir.clone(), node.file.clone());
                        h_flex()
                            .gap_0p5()
                            .p_0p5()
                            .rounded(theme.radius)
                            .bg(theme.secondary)
                            .child(toggle("tv-src", t!("tv.source").to_string(), !compiled && !blame_on, false, cx))
                            .child(toggle("tv-cmp", t!("tv.compiled").to_string(), compiled, true, cx).disabled(!has_compiled).when(!has_compiled, |b| b.tooltip(t!("tv.no_compiled").to_string())))
                            .child(Button::new("tv-blame").label("Blame").ghost().xsmall().selected(blame_on).on_click(cx.listener(move |t, _, _, cx| {
                                t.show_compiled = false;
                                if !t.show_blame {
                                    t.toggle_blame(dir.clone(), file.clone(), cx);
                                }
                                cx.notify();
                            })))
                    })
                    // …then what to do with it, after a divider.
                    .child(div().w(px(1.)).h(px(16.)).bg(theme.border))
                    .child(
                        Button::new("tv-compile")
                            .label(if self.compiling { t!("tv.compiling").to_string() } else { t!("tv.compile").to_string() })
                            .xsmall()
                            .outline()
                            .loading(self.compiling)
                            .tooltip(t!("tv.compile_tip").to_string())
                            .on_click(cx.listener(move |t, _, _, cx| t.compile(compile_node.clone(), cx))),
                    )
                    .when_some(url, |d, u| d.child(Button::new("tv-gh").label(t!("tv.open_github").to_string()).outline().xsmall().on_click(move |_, _, cx| cx.open_url(&u))))
                    .child(Button::new("tv-ed").label(t!("tv.open_editor").to_string()).outline().xsmall().on_click(move |_, _, cx| {
                        // `status` in the background: waits for `open` to finish (no zombie) without blocking the UI.
                        let local = local.clone();
                        cx.background_spawn(async move {
                            let _ = std::process::Command::new("open").arg(&local).status();
                        })
                        .detach();
                    }))
                    .when(!self.dbt_details, |d| {
                        d.child(Button::new("tv-dbt-details").label(t!("tv.details").to_string()).outline().xsmall().on_click(cx.listener(|t, _, _, cx| {
                            t.dbt_details = true;
                            cx.notify();
                        })))
                    }),
            )
            .child(div().flex_1().min_h_0().child(if self.show_blame && !compiled { self.render_blame(&project, &node.file, cx) } else { Editor::new(&editor).h_full().into_any_element() }))
            .when(self.compiling && !self.compile_live.is_empty(), |d| {
                d.child(
                    v_flex()
                        .px_3()
                        .py_1()
                        .max_h(px(160.))
                        .overflow_hidden()
                        .bg(theme.secondary)
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .children(self.compile_live.iter().map(|l| div().whitespace_nowrap().overflow_hidden().text_ellipsis().child(l.clone()))),
                )
            })
            .when_some(
                match &self.compiled {
                    Some((u, Load::Failed(m))) if *u == node.unique_id => Some(m.clone()),
                    _ => None,
                },
                |d, m| {
                    // × drops the failure; the next draw falls back to the compiled SQL on disk, if any.
                    let close = Button::new("tv-compile-err-close").icon(IconName::Close).ghost().xsmall().tooltip(t!("tv.close_panel").to_string()).on_click(cx.listener(|t, _, _, cx| {
                        t.compiled = None;
                        cx.notify();
                    }));
                    d.child(
                        h_flex()
                            .items_start()
                            .border_t_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .id("tv-compile-err")
                                    .flex_1()
                                    .min_w_0()
                                    .px_3()
                                    .py_1()
                                    .max_h(px(160.))
                                    .overflow_y_scroll()
                                    .text_xs()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_color(theme.danger)
                                    .child(m),
                            )
                            .child(div().p_1().child(close)),
                    )
                },
            )
            .child(div().px_3().py_1().text_xs().text_color(theme.muted_foreground).child(match (compiled, compiled_sql.as_ref().and_then(|c| c.1)) {
                (true, Some(t)) => t!("tv.compiled_ago", ago = crate::ui::ago(t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0))).to_string(),
                _ => format!("{} @ {}", project.git.branch, project.git.sha),
            }));
        h_flex()
            .size_full()
            .items_start()
            .child(right)
            .when(self.dbt_details, |d| {
                let close = cx.listener(|t, _, _, cx| {
                    t.dbt_details = false;
                    cx.notify();
                });
                d.child(crate::frame::side_panel("dbt-aside", 380., close, left, cx))
            })
            .into_any_element()
    }
}

/// A step of "Comes from": dot in the type color (dbt model/source/seed/outside dbt).
fn lineage_step(n: &LinNode, project: Option<&DbtProject>, theme: &gpui_kit::component::Theme) -> Div {
    let found = project.and_then(|p| p.manifest.find_table(&n.db, &n.schema, &n.object)).map(|(f, _)| f);
    let kind = found.map(|f| f.resource_type.clone()).unwrap_or_default();
    let label = match found {
        Some(f) => format!("{}.{} · dbt {}", n.db, n.schema, f.resource_type),
        None => format!("{}.{} · {}", n.db, n.schema, t!("tv.outside_dbt")),
    };
    h_flex().gap_2().items_start().child(div().mt(px(5.)).size(px(7.)).flex_shrink_0().rounded_full().bg(kind_dot(&kind, theme))).child(
        v_flex()
            .min_w_0()
            .child(div().text_xs().font_family(theme.mono_font_family.clone()).child(format!("{}.{}", n.object, n.column)))
            .child(div().text_xs().text_color(theme.muted_foreground).child(label)),
    )
}

/// Hover card of a column's tags: name as the title, then aligned label/value rows.
fn tag_cards(tags: &[table_info::ColTag], cx: &App) -> AnyElement {
    let theme = cx.theme();
    let row = |label: String, value: String, strong: bool| {
        h_flex().gap_3().child(div().w(px(80.)).flex_shrink_0().text_color(theme.muted_foreground).child(label)).child(div().when(strong, |d| d.font_weight(FontWeight::SEMIBOLD)).child(value))
    };
    v_flex()
        .max_w(px(380.))
        .py_1()
        .gap_3()
        .text_xs()
        .children(tags.iter().enumerate().map(|(i, t)| {
            v_flex()
                .gap_1()
                .when(i > 0, |d| d.pt_3().border_t_1().border_color(theme.border))
                .child(h_flex().gap_1p5().pb_1().text_sm().font_weight(FontWeight::SEMIBOLD).child(Icon::new(IconName::Tag).xsmall().text_color(theme.muted_foreground)).child(t.name.clone()))
                .child(row(t!("tv.tag_value").to_string(), t.value.clone(), true))
                .child(row(t!("tv.tag_location").to_string(), format!("{}.{}", t.database, t.schema), false))
                .when_some(t.method.as_ref(), |d, m| {
                    let m = m.to_lowercase();
                    let m = m[..1].to_uppercase() + &m[1..];
                    d.child(row(t!("tv.tag_assigned_by").to_string(), m, false))
                })
        }))
        .into_any_element()
}

/// dbt tests like the tag pills: first test short-named, "+N", hover lists the full names.
fn tests_cell(i: usize, tests: &[String], cx: &App) -> AnyElement {
    let Some(first) = tests.first() else { return div().into_any_element() };
    let theme = cx.theme();
    let pill = || h_flex().h(px(22.)).px_2().gap_1().rounded_full().border_1().border_color(theme.border).text_xs();
    let tip = tests.join("\n");
    h_flex()
        .id(("def-tests", i))
        .gap_1()
        .min_w_0()
        .tooltip(move |w, cx| Tooltip::new(tip.clone()).build(w, cx))
        .child(
            pill()
                .min_w_0()
                .child(Icon::new(IconName::CircleCheck).xsmall().text_color(theme.success))
                .child(div().min_w_0().overflow_hidden().text_ellipsis().child(table_info::short_test_name(first))),
        )
        .when(tests.len() > 1, |d| d.child(pill().flex_shrink_0().text_color(theme.muted_foreground).child(format!("+{}", tests.len() - 1))))
        .into_any_element()
}

/// Classification level from the org config ("C3"), outlined; the color of the text grows with the
/// level and the lock says a masking policy covers the column. Hover: the level's label.
fn class_badge(level: Option<u8>, masked: bool, org: &rb_core::org_config::OrgConfig, cx: &App) -> AnyElement {
    let Some(l) = level else { return div().into_any_element() };
    let theme = cx.theme();
    let color = match org.level_color(l) {
        LevelColor::Muted => theme.muted_foreground,
        LevelColor::Info => theme.info,
        LevelColor::Success => theme.success,
        LevelColor::Warning => theme.warning,
        LevelColor::Danger => theme.danger,
        LevelColor::Rgb(c) => rgb(c).into(),
    };
    let tip = org.level_label(l).map_or(format!("C{l}"), |n| format!("C{l} · {n}"));
    h_flex()
        .id(SharedString::from(format!("def-class-{l}-{masked}")))
        .h(px(22.))
        .px_2()
        .gap_1()
        .rounded_full()
        .border_1()
        .border_color(color.opacity(0.5))
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .tooltip(move |w, cx| Tooltip::new(tip.clone()).build(w, cx))
        .child(format!("C{l}"))
        .when(masked, |d| d.child(Icon::new(IconName::Lock).xsmall()))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    #[test]
    fn lags_read_like_people_say_them() {
        assert_eq!((super::lag_label(3600), super::lag_label(7200), super::lag_label(900), super::lag_label(40)), ("1 h".into(), "2 h".into(), "15 min".into(), "40.0 s".into()));
    }
}
