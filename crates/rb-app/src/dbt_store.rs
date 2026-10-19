//! The connection's dbt project: loads the manifest in the background, runs `dbt parse` on demand and
//! draws the status card (connection dialog and the table's dbt section).

use rust_i18n::t;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::config::DbtConfig;
use rb_core::dbt::{self, DbtProject};
use rb_core::user_view;

use crate::admin_tab::Load;

pub struct DbtStore {
    pub cfg: Option<DbtConfig>,
    pub project: Load<Arc<DbtProject>>,
    pub refreshing: bool,
    /// Output of the last `dbt parse` that failed.
    pub refresh_error: Option<String>,
    /// `aws sso login` in progress (opens the browser).
    pub aws_logging_in: bool,
    /// Last lines of the running `dbt parse` (updated every half second).
    pub live: Vec<String>,
    /// Profiles from ~/.aws/config and the shell's, for the login recommendation.
    aws_profiles: Vec<String>,
    shell_profile: Option<String>,
    generation: u64,
}

impl DbtStore {
    pub fn new(cfg: Option<DbtConfig>, cx: &mut Context<Self>) -> Self {
        let mut s = DbtStore { cfg, project: Load::Loading, refreshing: false, refresh_error: None, aws_logging_in: false, live: Vec::new(), aws_profiles: Vec::new(), shell_profile: None, generation: 0 };
        s.reload(cx);
        s
    }

    /// Different folder/target (dialog): reload.
    pub fn set_cfg(&mut self, cfg: Option<DbtConfig>, cx: &mut Context<Self>) {
        if cfg != self.cfg {
            self.cfg = cfg;
            self.refresh_error = None;
            self.reload(cx);
        }
    }

    /// `aws sso login --profile p` and, if it succeeds, runs Refresh with that profile.
    pub fn aws_login(&mut self, profile: String, cx: &mut Context<Self>) {
        if self.aws_logging_in {
            return;
        }
        self.aws_logging_in = true;
        if let Some(c) = self.cfg.as_mut() {
            c.aws_profile = Some(profile.clone());
        }
        cx.notify();
        let task = cx.background_spawn(async move { dbt::aws_login(&profile) });
        cx.spawn(async move |this, cx| {
            let res = task.await;
            this.update(cx, |s, cx| {
                s.aws_logging_in = false;
                match res {
                    Ok(()) => s.refresh(cx),
                    Err(e) => s.refresh_error = Some(e.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// While typing: stores the folder/target without reloading (Refresh uses the current value).
    pub fn set_cfg_quiet(&mut self, cfg: Option<DbtConfig>) {
        self.cfg = cfg;
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let Some(cfg) = self.cfg.clone() else {
            self.project = Load::Failed(t!("dbt.not_configured").to_string());
            cx.notify();
            return;
        };
        self.project = Load::Loading;
        cx.notify();
        // Large manifest: read and deserialized off the UI thread.
        let task = cx.background_spawn(async move { dbt::load(&cfg) });
        cx.spawn(async move |this, cx| {
            let res = task.await;
            this.update(cx, |s, cx| {
                if s.generation == generation {
                    s.project = match res {
                        Ok(p) => Load::Ready(Arc::new(p)),
                        Err(e) => Load::Failed(e.to_string()),
                    };
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// `dbt parse --target …` and then reloads.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(cfg) = self.cfg.clone() else { return };
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        self.refresh_error = None;
        self.live.clear();
        cx.notify();
        let log = dbt::log_path();
        let log2 = log.clone();
        let task = cx.background_spawn(async move { dbt::refresh_logged(&cfg, &log2) });
        // Follows the log while dbt runs.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                let tail = dbt::log_tail(&log, 12);
                let go = this
                    .update(cx, |s, cx| {
                        if !tail.is_empty() {
                            s.live = tail;
                            cx.notify();
                        }
                        s.refreshing
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
            this.update(cx, |s, cx| {
                s.refreshing = false;
                s.live.clear();
                match res {
                    Ok(()) => s.reload(cx),
                    Err(e) => {
                        let msg = e.to_string();
                        // Expired AWS SSO: loads the profiles to recommend the login.
                        if dbt::needs_aws_login(&msg) && s.aws_profiles.is_empty() {
                            s.aws_profiles = dbt::read_aws_profiles();
                            s.shell_profile = dbt::shell_aws_profile();
                        }
                        s.refresh_error = Some(msg);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// "generated 3 h ago".
pub fn parsed_ago(p: &DbtProject) -> String {
    let Some(secs) = p.parsed_at.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64) else { return String::new() };
    let s = user_view::span(user_view::now() - secs);
    let n = match s {
        user_view::Span::Minutes(n) => t!("span.minutes", n = n),
        user_view::Span::Hours(n) => t!("span.hours", n = n),
        user_view::Span::Days(n) => t!("span.days", n = n),
        user_view::Span::Months(n) => t!("span.months", n = n),
        user_view::Span::Years(n) => t!("span.years", n = n),
    };
    t!("dbt.parsed_ago", ago = n).to_string()
}

impl Render for DbtStore {
    /// The status card from board 10.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let refresh = Button::new("dbt-refresh")
            .label(if self.refreshing { t!("dbt.refreshing").to_string() } else { t!("dbt.refresh").to_string() })
            .outline()
            .xsmall()
            .loading(self.refreshing)
            .on_click(cx.listener(|s, _, _, cx| s.refresh(cx)));
        let mut card = v_flex().gap_2().p_3().rounded(theme.radius_lg).border_1().border_color(theme.border).text_sm();
        card = match &self.project {
            _ if self.cfg.is_none() => card.child(div().text_color(theme.muted_foreground).child(t!("dbt.pick_folder").to_string())),
            Load::Loading => card.child(div().text_color(theme.muted_foreground).child(t!("tree.loading").to_string())),
            Load::Ready(p) => {
                let (models, sources, seeds) = p.manifest.counts();
                card.child(h_flex().gap_2().child(div().text_color(theme.success).child("✓")).child(format!("{} · {}", t!("dbt.manifest_found"), parsed_ago(p))).child(div().flex_1()).child(refresh))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(t!("dbt.counts", models = models, sources = sources, seeds = seeds).to_string()))
                    .when_some(p.git.repo.clone(), |d, r| d.child(div().text_xs().text_color(theme.muted_foreground).child(t!("dbt.github", repo = r.trim_start_matches("https://"), branch = p.git.branch).to_string())))
            }
            Load::Failed(m) => card.child(h_flex().gap_2().child(div().flex_1().text_color(theme.warning).child(m.clone())).child(refresh)),
        };
        // Live output of the running dbt parse.
        if self.refreshing && !self.live.is_empty() {
            card = card.child(
                v_flex()
                    .p_2()
                    .rounded(theme.radius)
                    .bg(theme.secondary)
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .children(self.live.iter().map(|l| div().overflow_hidden().text_ellipsis().whitespace_nowrap().child(l.clone()))),
            );
        }
        // Recommendation: log in to AWS with the right profile and try again.
        if let Some(out) = self.refresh_error.clone().filter(|o| dbt::needs_aws_login(o)) {
            let current = self.cfg.as_ref().and_then(|c| c.aws_profile.clone()).or_else(|| self.shell_profile.clone());
            let mut picks = h_flex().flex_wrap().gap_1();
            // The current one first, then all in alphabetical order.
            let mut profiles = self.aws_profiles.clone();
            profiles.sort_by_key(|p| (current.as_deref() != Some(p.as_str()), p.clone()));
            for (i, p) in profiles.into_iter().enumerate() {
                let sel = current.as_deref() == Some(p.as_str());
                let p2 = p.clone();
                picks = picks.child(Button::new(("aws-p", i)).label(p).xsmall().outline().selected(sel).on_click(cx.listener(move |s, _, _, cx| {
                    if let Some(c) = s.cfg.as_mut() {
                        c.aws_profile = Some(p2.clone());
                    }
                    cx.notify();
                })));
            }
            let login_profile = current.clone();
            card = card.child(
                v_flex()
                    .gap_2()
                    .p_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.warning)
                    .child(div().text_color(theme.warning).child(t!("dbt.aws_expired").to_string()))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(t!("dbt.aws_pick").to_string()))
                    .child(picks)
                    .child(
                        Button::new("aws-login")
                            .label(match &current {
                                Some(p) => t!("dbt.aws_login", profile = p).to_string(),
                                None => t!("dbt.aws_login_pick").to_string(),
                            })
                            .primary()
                            .small()
                            .loading(self.aws_logging_in)
                            .disabled(login_profile.is_none())
                            .on_click(cx.listener(move |s, _, _, cx| {
                                if let Some(p) = login_profile.clone() {
                                    s.aws_login(p, cx);
                                }
                            })),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(t!("dbt.aws_note", out = out.lines().last().unwrap_or("")).to_string())),
            );
        } else if let Some(out) = self.refresh_error.clone() {
            let copy = out.clone();
            card = card.child(
                v_flex()
                    .gap_1()
                    .child(div().text_color(theme.danger).child(t!("dbt.parse_failed").to_string()))
                    .child(div().id("dbt-out").p_2().rounded(theme.radius).bg(theme.secondary).text_xs().font_family(theme.mono_font_family.clone()).max_h(px(200.)).overflow_y_scroll().child(out))
                    .child(Button::new("dbt-copy").label(t!("dbt.copy_output").to_string()).ghost().xsmall().on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone())))),
            );
        }
        let target = self.cfg.as_ref().map(|c| c.target.clone()).unwrap_or_else(|| "prod".into());
        card.child(div().text_xs().text_color(theme.muted_foreground).child(t!("dbt.parse_note", target = target).to_string()))
    }
}
