//! Quick-open index on Snowflake: own session, rebuilt every 30 s; the diff fixes the tree.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::*;
use rb_core::quick_open::{SfIndex, diff, sf_snapshot};
use rb_core::session::Session;

use crate::Db;
use crate::meta::{ConnTarget, MetaStore};

const TICK: Duration = Duration::from_secs(30);

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
            if ix.index.is_none() && ix.error.is_none() && !meta.read(cx).cache.databases.is_empty() {
                ix.refresh(cx);
            }
        });
        Indexer { index: None, running: false, error: None, conn, session: None, meta, _tick: tick, _meta: sub }
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
