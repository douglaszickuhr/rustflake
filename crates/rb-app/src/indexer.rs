//! Quick-open index on Snowflake: own session, rebuilt every few minutes and when ⌘K opens a stale one;
//! the diff fixes the tree.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::*;
use rb_core::quick_open::{SfIndex, diff, sf_snapshot};
use rb_core::session::Session;

use crate::Db;
use crate::meta::{ConnTarget, MetaStore};

/// `SHOW … IN ACCOUNT` costs cloud-services credits: the background refresh is rare.
const TICK: Duration = Duration::from_secs(5 * 60);
/// Older than this, opening ⌘K refreshes it.
const STALE: Duration = Duration::from_secs(30);

pub struct Indexer {
    pub index: Option<SfIndex>,
    pub running: bool,
    pub error: Option<String>,
    conn: ConnTarget,
    session: Option<Arc<Session>>,
    meta: Entity<MetaStore>,
    _tick: Task<()>,
    _meta: Subscription,
}

impl Indexer {
    pub fn new(conn: ConnTarget, meta: Entity<MetaStore>, cx: &mut Context<Self>) -> Self {
        let tick = cx.spawn(async move |this, cx| {
            loop {
                if this.update(cx, |ix, cx| ix.refresh(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(TICK).await;
            }
        });
        // The first load waits for the database list (the per-database fallback and the filter depend on
        // it); the tick may arrive earlier and is skipped in `refresh`.
        let sub = cx.observe(&meta, |ix, meta, cx| {
            // A saved index counts as nothing to wait for: its age makes the tick refresh it.
            if ix.index.as_ref().is_none_or(|i| i.refreshed.is_none_or(|t| t.elapsed() > STALE)) && !ix.running && ix.error.is_none() && !meta.read(cx).cache.databases.is_empty() {
                ix.refresh(cx);
            }
        });
        // Last run's index: search works at once; the refresh below replaces it.
        let index = rb_core::quick_open::load_index(&conn.cfg.id);
        Indexer { index, running: false, error: None, conn, session: None, meta, _tick: tick, _meta: sub }
    }

    pub fn refresh_if_stale(&mut self, cx: &mut Context<Self>) {
        if self.index.as_ref().and_then(|i| i.refreshed).is_none_or(|t| t.elapsed() > STALE) {
            self.refresh(cx);
        }
    }

    /// New snapshot; skipped if the previous one is still running.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.running || self.meta.read(cx).cache.databases.is_empty() {
            return;
        }
        self.running = true;
        cx.notify();
        let (conn, existing, engine) = (self.conn.clone(), self.session.clone(), cx.global::<Db>().0.clone());
        let dbs: Vec<String> = self.meta.read(cx).cache.visible_databases().cloned().collect();
        cx.spawn(async move |this, cx| {
            let res = async {
                // Own session (same login): does not join the tree and autocomplete queue.
                let s = match existing {
                    Some(s) => s,
                    None => Arc::new(engine.connect(&conn.cfg, conn.creds.clone()).await?),
                };
                let idx = sf_snapshot(&s, &dbs).await?;
                Ok::<_, rb_core::DbError>((s, idx))
            }
            .await;
            this.update(cx, |ix, cx| {
                ix.running = false;
                match res {
                    Ok((s, new)) => {
                        ix.session = Some(s);
                        if let Some(old) = &ix.index {
                            let d = diff(old, &new);
                            ix.meta.update(cx, |m, cx| m.apply_index_diff(d, cx));
                        }
                        // Kept for the next launch (off the UI thread).
                        let (id, saved) = (ix.conn.cfg.id.clone(), new.clone());
                        cx.background_spawn(async move {
                            if let Err(e) = rb_core::quick_open::save_index(&id, &saved) {
                                tracing::warn!("quick-open index not saved: {e}");
                            }
                        })
                        .detach();
                        ix.index = Some(new);
                        ix.error = None;
                    }
                    Err(e) => {
                        tracing::warn!("quick-open index: {e}");
                        ix.error = Some(e.to_string());
                    }
                }
                cx.notify();
            })
        })
        .detach();
    }
}
