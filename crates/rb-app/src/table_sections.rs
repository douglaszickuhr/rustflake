//! Snowflake table view: header and Definition / dbt sections (canvas boards 7, 9 and 10).

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::dbt::{DbtProject, Node, commit_url, file_url};
use rb_core::explain::short_count;
use rb_core::table_info::{self, DescSource, LinNode};
use rb_core::org_config::TabKind;
use rb_core::user_view;
use rust_i18n::t;

use super::{Section, TableTab};
use crate::Db;
use crate::admin_tab::Load;
use crate::dbt_store::parsed_ago;

/// Node type color (dbt model / source / seed / outside dbt), same as lineage.
fn kind_dot(kind: &str, theme: &gpui_kit::component::Theme) -> Hsla {
    match kind {
        "model" | "snapshot" => theme.primary,
        "source" => theme.success,
        "seed" => theme.info,
        _ => theme.warning,
    }
}

/// "3 months ago" from the commit's Unix second.
fn when_from(secs: i64) -> String {
    let n = match user_view::span(user_view::now() - secs) {
        user_view::Span::Minutes(n) => t!("span.minutes", n = n),
        user_view::Span::Hours(n) => t!("span.hours", n = n),
        user_view::Span::Days(n) => t!("span.days", n = n),
        user_view::Span::Months(n) => t!("span.months", n = n),
        user_view::Span::Years(n) => t!("span.years", n = n),
    };
    t!("admin.ago", ago = n).to_string()
}

fn ago(ts: &str) -> Option<String> {
    let t = user_view::parse_ts(ts)?;
    let n = match user_view::span(user_view::now() - t) {
        user_view::Span::Minutes(n) => t!("span.minutes", n = n),
        user_view::Span::Hours(n) => t!("span.hours", n = n),
        user_view::Span::Days(n) => t!("span.days", n = n),
        user_view::Span::Months(n) => t!("span.months", n = n),
        user_view::Span::Years(n) => t!("span.years", n = n),
    };
    Some(t!("admin.ago", ago = n).to_string())
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
        let (conn, engine) = (self.conn.clone(), cx.global::<Db>().0.clone());
        cx.spawn(async move |this, cx| {
            let res = match engine.connect(&conn.cfg, conn.creds.clone()).await {
                Ok(s) => run(&s).await,
                Err(e) => Err(e),
            };
            this.update(cx, |t, cx| {
                done(t, res, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
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
            |t, res, _| t.facts = Some(res.map_or_else(|e| Load::Failed(e.to_string()), Load::Ready)),
        );
    }

    fn ensure_definition(&mut self, cx: &mut Context<Self>) {
        if self.def_raw.is_some() {
            return;
        }
        self.def_raw = Some(Load::Loading);
        let (db, schema) = self.db_schema();
        let table = self.table.clone();
        let org = self.conn.org.clone();
        self.spawn_query(
            cx,
            async move |s| {
                let (n, r) = s.query_named(&table_info::columns_sql(&db, &schema, &table)).await?;
                let cols = table_info::parse_columns(&n, &r);
                // Tags and policies are extras: without permission, the columns table stays and the
                // summary says classification didn't load (instead of looking like "nothing classified").
                let mut err = None;
                let (tags, others) = match s.query_named(&table_info::tags_sql(&db, &schema, &table)).await {
                    Ok((n, r)) => (table_info::parse_class_tags(&org, &n, &r), table_info::parse_all_tags(&org, &n, &r)),
                    Err(e) => {
                        err = Some(e.to_string());
                        (HashMap::new(), HashMap::new())
                    }
                };
                let masks = match s.query_named(&table_info::policies_sql(&db, &schema, &table)).await {
                    Ok((n, r)) => table_info::parse_masking(&n, &r),
                    Err(e) => {
                        err.get_or_insert(e.to_string());
                        HashMap::new()
                    }
                };
                Ok((cols, tags, masks, others, err))
            },
            |t, res, _| t.def_raw = Some(res.map_or_else(|e| Load::Failed(e.to_string()), Load::Ready)),
        );
    }

    fn select_col(&mut self, col: String, cx: &mut Context<Self>) {
        self.selected_col = Some(col.clone());
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
        }
        cx.notify();
    }

    fn go(&mut self, s: Section, cx: &mut Context<Self>) {
        self.section = s;
        if s == Section::Definition {
            self.ensure_definition(cx);
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
        let matched = project.as_deref().and_then(|p| self.node_match(p)).map(|(n, exact)| (n.clone(), exact));
        let (node, by_name) = match matched {
            Some((n, exact)) => (Some(n), !exact),
            None => (None, false),
        };
        // Same qualifier as the grid: quotes for lowercase/spaced names.
        let fq = rb_core::dialect::Dialect::Snowflake.qualified(&self.schema, &self.table);
        let open_query = self.open_query.clone();
        let mut top = h_flex()
            .gap_3()
            .flex_wrap()
            .items_center()
            .child(div().text_xl().font_family(theme.mono_font_family.clone()).child(self.table.clone()))
            .child(div().text_sm().font_family(theme.mono_font_family.clone()).text_color(theme.muted_foreground).child(format!("{db}.{schema}")));
        if let Some(n) = &node {
            let badge = match &n.materialized {
                Some(m) => format!("{} · {m}", n.resource_type),
                None => n.resource_type.clone(),
            };
            top = top
                .child(Tag::info().small().child(format!("dbt {badge}")))
                .when(by_name, |d| d.child(div().text_xs().text_color(theme.warning).child(t!("tv.matched_by_name").to_string())))
                .when_some(n.owner.clone(), |d, o| d.child(div().text_sm().text_color(theme.muted_foreground).child(format!("{} {o}", t!("tv.owner")))))
                .children(n.tags.iter().map(|tg| Tag::secondary().small().child(tg.clone())));
        }
        top = top.child(div().flex_1()).child(Button::new("tv-query").label(t!("tv.query").to_string()).outline().small().on_click(move |_, w, cx| open_query(format!("select * from {fq} limit 100;"), w, cx)));
        let facts = match &self.facts {
            Some(Load::Ready(f)) => {
                let kind = match f.kind.as_str() {
                    "BASE TABLE" => t!("tv.kind_table").to_string(),
                    "VIEW" => t!("tv.kind_view").to_string(),
                    "MATERIALIZED VIEW" => t!("tv.kind_matview").to_string(),
                    other => other.to_lowercase(),
                };
                let mut v = vec![kind];
                v.extend(f.rows.map(|r| t!("tv.rows", n = short_count(r as f64)).to_string()));
                v.extend(f.bytes.map(|b| format!("{}B", short_count(b as f64))));
                v.extend(f.last_altered.as_deref().and_then(ago).map(|a| t!("tv.changed", ago = a).to_string()));
                v
            }
            Some(Load::Failed(m)) => vec![m.clone()],
            _ => vec![t!("tree.loading").to_string()],
        };
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
                    .child(tab("tv-data", t!("tv.data").to_string(), Section::Data, cur, cx))
                    .child(tab("tv-def", t!("tv.definition").to_string(), Section::Definition, cur, cx))
                    .child(tab("tv-dbt", "dbt".into(), Section::Dbt, cur, cx))
                    .children(self.custom.iter().enumerate().map(|(i, c)| {
                        let s = Section::Custom(i);
                        Button::new(("tv-custom", i)).label(c.name.clone()).ghost().small().selected(s == cur).on_click(cx.listener(move |t, _, _, cx| t.go(s, cx)))
                    })),
            )
    }

    /// Board 7: columns with description (dbt or comment), classification, masking and tests.
    pub(crate) fn render_definition(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let (cols, tags, masks, others, tag_err) = match &self.def_raw {
            Some(Load::Ready(r)) => r.clone(),
            Some(Load::Failed(m)) => return div().p_4().text_color(theme.danger).child(m.clone()).into_any_element(),
            _ => return div().p_4().text_color(theme.muted_foreground).child(t!("tree.loading").to_string()).into_any_element(),
        };
        // Without a classification tag in the config, "not classified" would be false.
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
        let mono = theme.mono_font_family.clone();
        let mut table = v_flex().child(
            h_flex()
                .px_3()
                .py_2()
                .gap_3()
                .text_xs()
                .text_color(theme.muted_foreground)
                .border_b_1()
                .border_color(theme.border)
                .child(div().w(px(200.)).child(t!("tv.col_column").to_string()))
                .child(div().w(px(130.)).child(t!("tv.col_type").to_string()))
                .child(div().flex_1().child(t!("tv.col_desc").to_string()))
                .child(div().w(px(110.)).child(t!("tv.col_class").to_string()))
                .child(div().w(px(180.)).child(t!("tv.col_tests").to_string())),
        );
        for (i, r) in rows.iter().enumerate() {
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
                (None, _) => div().text_color(theme.warning).child(t!("tv.no_desc").to_string()),
            };
            let class = match r.level {
                Some(l) => {
                    let tag = if l >= 3 { Tag::warning() } else { Tag::secondary() };
                    tag.small().child(h_flex().gap_1().child(format!("C{l}")).when(r.masked_by.is_some(), |d| d.child(Icon::new(IconName::Lock).xsmall()))).into_any_element()
                }
                None if !has_class => div().into_any_element(),
                // Empty looked like "still loading".
                None => div().text_xs().text_color(theme.muted_foreground).child(t!("tv.not_classified").to_string()).into_any_element(),
            };
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
                    .child(div().w(px(200.)).font_family(mono.clone()).overflow_hidden().text_ellipsis().child(name.clone()))
                    .child(div().w(px(130.)).font_family(mono.clone()).text_color(theme.muted_foreground).overflow_hidden().text_ellipsis().child(r.col.kind.clone()))
                    .child(div().flex_1().min_w_0().overflow_hidden().child(desc))
                    .child(div().w(px(110.)).child(class))
                    .child(h_flex().w(px(180.)).flex_wrap().gap_1().children(r.tests.iter().map(|tn| Tag::info().small().child(tn.clone()))))
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
            .when(has_class, |d| d.child("·").child(t!("tv.sum_classified", n = sm.classified).to_string()))
            .child("·")
            .child(t!("tv.sum_tests", n = sm.tests).to_string())
            .when(sm.undocumented > 0, |d| d.child("·").child(div().text_color(theme.warning).child(t!("tv.sum_undocumented", n = sm.undocumented).to_string())))
            .when_some(tag_err, |d, e| d.child("·").child(div().text_color(theme.danger).child(t!("tv.class_unavailable", error = e).to_string())));
        let selected = self.selected_col.clone().and_then(|c| rows.iter().find(|r| r.col.name == c).cloned());
        let aside = selected.map(|r| {
            let extra = others.iter().find(|(k, _)| k.eq_ignore_ascii_case(&r.col.name)).map(|(_, v)| v.clone()).unwrap_or_default();
            self.render_column_panel(&r, &extra, project.as_deref(), cx)
        });
        h_flex()
            .size_full()
            .items_start()
            .child(v_flex().id("def-main").flex_1().min_w_0().h_full().overflow_y_scroll().overflow_x_hidden().p_3().child(summary).child(table))
            .when_some(aside, |d, a| d.child(div().id("def-aside").w(px(360.)).h_full().overflow_y_scroll().border_l_1().border_color(theme.border).p_4().child(a)))
            .into_any_element()
    }

    fn render_column_panel(&mut self, r: &table_info::DefRow, extra_tags: &[(String, String)], project: Option<&DbtProject>, cx: &mut Context<Self>) -> Div {
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
            for (k, v) in extra_tags {
                out = out.child(h_flex().gap_2().text_xs().child(div().font_family(theme.mono_font_family.clone()).child(k.clone())).child(div().text_color(theme.muted_foreground).child(v.clone())));
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
                out = out.child(
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(Tag::info().small().child(kind.clone()))
                        .when_some(status, |d, (s, when)| d.child(div().text_xs().text_color(match s.as_str() {
                            "pass" => theme.success,
                            "warn" => theme.warning,
                            "skipped" => theme.muted_foreground,
                            _ => theme.danger,
                        }).child(format!("{s} · {}", ago(&when.replace('T', " ")).unwrap_or_default())))),
                );
            }
        }
        out
    }

    /// `dbt compile --select <modelo>` with live output; when done, shows the compiled SQL.
    fn compile(&mut self, node: Node, cx: &mut Context<Self>) {
        let Some(cfg) = self.dbt.as_ref().and_then(|d| d.read(cx).cfg.clone()) else { return };
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
                    let who = if l.uncommitted { t!("tv.uncommitted").to_string() } else { format!("{} · {}", l.author, when_from(l.time)) };
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
            let what = t.detail.clone().map(|d| if t.kind == "accepted_values" { t!("tv.n_values", n = d).to_string() } else { d }).or_else(|| t.column.clone()).unwrap_or_else(|| t!("tv.model_level").to_string());
            match by_kind.iter_mut().find(|(k, _)| *k == t.kind) {
                Some((_, v)) => v.push(what),
                None => by_kind.push((t.kind.clone(), vec![what])),
            }
        }
        let head = |s: String| div().pt_2().text_xs().text_color(theme.muted_foreground).child(s.to_uppercase());
        let row = |k: String, v: String| h_flex().gap_3().py_1().text_sm().border_b_1().border_color(theme.border).child(div().w(px(110.)).flex_shrink_0().text_color(theme.muted_foreground).child(k)).child(div().min_w_0().child(v));
        let mut left = v_flex()
            .gap_2()
            .child(head(t!("tv.col_desc").to_string()))
            .child(div().text_sm().child(if node.description.is_empty() { t!("tv.no_desc").to_string() } else { node.description.clone() }));
        let mat = [node.materialized.clone(), node.strategy.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
        left = left
            .when(!mat.is_empty(), |d| d.child(row(t!("tv.materialized").to_string(), mat)))
            .when(!node.unique_key.is_empty(), |d| d.child(row(t!("tv.unique_key").to_string(), node.unique_key.join(", "))))
            .when_some(node.owner.clone(), |d, o| d.child(row(t!("tv.owner").to_string(), o)))
            .when(node.meta.iter().any(|(k, _)| k != "owner"), |d| {
                // One line per key (already flattened: `atlan.attributes.ownerUsers`).
                let mut list = v_flex().gap_0p5().min_w_0();
                for (k, v) in node.meta.iter().filter(|(k, _)| k != "owner") {
                    list = list.child(v_flex().child(div().text_xs().font_family(theme.mono_font_family.clone()).text_color(theme.muted_foreground).child(k.clone())).child(div().text_sm().child(v.clone())));
                }
                d.child(h_flex().gap_3().py_1().items_start().border_b_1().border_color(theme.border).child(div().w(px(110.)).flex_shrink_0().text_sm().text_color(theme.muted_foreground).child(t!("tv.meta").to_string())).child(list))
            })
            .when(!node.tags.is_empty(), |d| d.child(h_flex().gap_1().py_1().child(div().w(px(110.)).text_sm().text_color(theme.muted_foreground).child(t!("tv.tags").to_string())).children(node.tags.iter().map(|t| Tag::secondary().small().child(t.clone())))));
        left = left.child(head(t!("tv.tests_n", n = tests.len()).to_string()));
        for (kind, cols) in by_kind {
            left = left.child(h_flex().gap_2().items_start().text_sm().child(Tag::info().small().child(kind)).child(div().min_w_0().text_xs().font_family(theme.mono_font_family.clone()).text_color(theme.muted_foreground).child(cols.join(", "))));
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
                .child(Button::new("tv-refresh").label(t!("dbt.refresh").to_string()).outline().xsmall().loading(store.read(cx).refreshing).on_click(move |_, _, cx| refresh_store.update(cx, |s, cx| s.refresh(cx)))),
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
                let e = cx.new(|cx| EditorState::new(window, cx).language("sql").default_value(code));
                self.sql_view = Some((e.clone(), key));
                e
            }
        };
        let url = file_url(&project.git, &node.file);
        let local = project.dir.join(&node.file);
        let has_compiled = compiled_sql.is_some();
        let compile_node = node.clone();
        let toggle = |id: &'static str, label: String, on: bool, value: bool, cx: &mut Context<Self>| {
            Button::new(id).label(label).xsmall().outline().selected(on).on_click(cx.listener(move |t, _, _, cx| {
                t.show_compiled = value;
                cx.notify();
            }))
        };
        let right = v_flex()
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
                    .child(div().flex_1().min_w_0().text_xs().font_family(theme.mono_font_family.clone()).text_color(theme.muted_foreground).overflow_hidden().text_ellipsis().child(node.file.clone()))
                    .child(Button::new("tv-blame").label("Blame").xsmall().outline().selected(self.show_blame).on_click({
                        let (dir, file) = (project.dir.clone(), node.file.clone());
                        cx.listener(move |t, _, _, cx| t.toggle_blame(dir.clone(), file.clone(), cx))
                    }))
                    .child(Button::new("tv-compile").label(if self.compiling { t!("tv.compiling").to_string() } else { t!("tv.compile").to_string() }).xsmall().outline().loading(self.compiling).tooltip(t!("tv.compile_tip").to_string()).on_click(cx.listener(move |t, _, _, cx| t.compile(compile_node.clone(), cx))))
                    .child(toggle("tv-src", t!("tv.source").to_string(), !compiled, false, cx))
                    .child(toggle("tv-cmp", t!("tv.compiled").to_string(), compiled, true, cx).disabled(!has_compiled).when(!has_compiled, |b| b.tooltip(t!("tv.no_compiled").to_string())))
                    .when_some(url, |d, u| d.child(Button::new("tv-gh").label(t!("tv.open_github").to_string()).outline().xsmall().on_click(move |_, _, cx| cx.open_url(&u))))
                    .child(Button::new("tv-ed").label(t!("tv.open_editor").to_string()).outline().xsmall().on_click(move |_, _, cx| {
                        // `status` in the background: waits for `open` to finish (no zombie) without blocking the UI.
                        let local = local.clone();
                        cx.background_spawn(async move {
                            let _ = std::process::Command::new("open").arg(&local).status();
                        })
                        .detach();
                    })),
            )
            .child(div().flex_1().min_h_0().child(if self.show_blame && !compiled { self.render_blame(&project, &node.file, cx) } else { Editor::new(&editor).h_full().into_any_element() }))
            .when(self.compiling && !self.compile_live.is_empty(), |d| {
                d.child(v_flex().px_3().py_1().max_h(px(160.)).overflow_hidden().bg(theme.secondary).text_xs().font_family(theme.mono_font_family.clone()).children(self.compile_live.iter().map(|l| div().whitespace_nowrap().overflow_hidden().text_ellipsis().child(l.clone()))))
            })
            .when_some(match &self.compiled {
                Some((u, Load::Failed(m))) if *u == node.unique_id => Some(m.clone()),
                _ => None,
            }, |d, m| d.child(div().id("tv-compile-err").px_3().py_1().max_h(px(160.)).overflow_y_scroll().text_xs().font_family(theme.mono_font_family.clone()).text_color(theme.danger).child(m)))
            .child(div().px_3().py_1().text_xs().text_color(theme.muted_foreground).child(match (compiled, compiled_sql.as_ref().and_then(|c| c.1)) {
                (true, Some(t)) => t!("tv.compiled_ago", ago = when_from(t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0))).to_string(),
                _ => format!("{} @ {}", project.git.branch, project.git.sha),
            }));
        h_flex()
            .size_full()
            .items_start()
            .child(v_flex().id("dbt-left").w(px(380.)).flex_shrink_0().h_full().overflow_y_scroll().p_4().border_r_1().border_color(theme.border).child(left))
            .child(right)
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
    h_flex()
        .gap_2()
        .items_start()
        .child(div().mt(px(5.)).size(px(7.)).flex_shrink_0().rounded_full().bg(kind_dot(&kind, theme)))
        .child(
            v_flex()
                .min_w_0()
                .child(div().text_xs().font_family(theme.mono_font_family.clone()).child(format!("{}.{}", n.object, n.column)))
                .child(div().text_xs().text_color(theme.muted_foreground).child(label)),
        )
}
