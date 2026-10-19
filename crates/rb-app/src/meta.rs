use std::collections::HashSet;
use std::sync::Arc;

use gpui_kit::*;
use rb_core::config::ConnectionConfig;
use rb_core::dialect::Dialect;
use rb_core::metadata::{self, ColumnMeta, IndexMeta, MetaCache, MetaRequest, SchemaObjects};
use rb_core::quick_open::{IndexDiff, refresh_plan};
use rb_core::org_config::OrgConfig;
use rb_core::session::{Credentials, Session};

#[derive(Clone)]
pub struct ConnTarget {
    pub cfg: ConnectionConfig,
    /// One copy per connection: tabs and tasks share it.
    pub creds: Arc<Credentials>,
    /// Company conventions (configuration chosen on the connection); with no configuration, `OrgConfig::none()`.
    pub org: Arc<OrgConfig>,
}

/// Dedicated session for metadata + cache shared by the tree and autocomplete.
pub struct MetaStore {
    pub session: Arc<Session>,
    pub cache: MetaCache,
    inflight: HashSet<MetaRequest>,
}

enum Loaded {
    Objects(SchemaObjects),
    Columns(Vec<ColumnMeta>),
    Indexes(Vec<IndexMeta>),
    Schemas(Vec<String>),
}

impl MetaStore {
    pub fn new(session: Arc<Session>, cx: &mut Context<Self>) -> Self {
        let cache = MetaCache { dialect: session.dialect(), ..Default::default() };
        let mut this = MetaStore { session, cache, inflight: HashSet::new() };
        this.reload_roots(cx);
        this
    }

    /// Reloads the tree root (schemas on Postgres, databases on Snowflake) and the context.
    pub fn reload_roots(&mut self, cx: &mut Context<Self>) {
        let s = self.session.clone();
        cx.spawn(async move |this, cx| {
            let res = async {
                let sf = s.dialect() == Dialect::Snowflake;
                let dbs = if sf { metadata::databases(&s).await? } else { Vec::new() };
                let schemas = if sf { None } else { Some(metadata::schemas(&s).await?) };
                Ok::<_, rb_core::DbError>((schemas, dbs, metadata::context(&s).await?))
            }
            .await;
            this.update(cx, |m, cx| {
                match res {
                    Ok((schemas, dbs, ctx)) => {
                        // On Snowflake the schemas arrive per database (MetaRequest::Schemas).
                        if let Some(schemas) = schemas {
                            m.cache.schemas = schemas;
                        }
                        m.cache.databases = dbs;
                        m.cache.search_path = ctx.search_path;
                        m.cache.warehouse = ctx.warehouse;
                        m.cache.role = ctx.role;
                    }
                    // The tree keeps what it had; the next expand or ↻ retries.
                    Err(e) => tracing::warn!("catalog roots: {e}"),
                }
                cx.notify();
            })
        })
        .detach();
    }

    /// Fixes the cache with the index diff: redoes on top whatever the tree already loaded (without clearing
    /// first, so columns and open nodes persist) and removes the schemas that disappeared.
    pub fn apply_index_diff(&mut self, d: IndexDiff, cx: &mut Context<Self>) {
        let plan = refresh_plan(&d, &self.cache);
        for s in plan.drop_schemas {
            self.cache.invalidate_schema(&s);
        }
        for req in plan.reload {
            self.fetch(req, cx);
        }
        if plan.reload_roots {
            self.reload_roots(cx);
        }
        cx.notify();
    }

    /// Some piece of the catalog is still on its way.
    pub fn loading(&self) -> bool {
        !self.inflight.is_empty()
    }

    /// Loads a piece of the catalog if it is not already cached or on its way.
    pub fn request(&mut self, req: MetaRequest, cx: &mut Context<Self>) {
        if !self.cache.has(&req) {
            self.fetch(req, cx);
        }
    }

    /// Fetches from the database even when the piece is cached; the response replaces what was there.
    fn fetch(&mut self, req: MetaRequest, cx: &mut Context<Self>) {
        if !self.inflight.insert(req.clone()) {
            return;
        }
        let s = self.session.clone();
        cx.spawn(async move |this, cx| {
            let res = match &req {
                MetaRequest::Objects(schema) => metadata::objects(&s, schema).await.map(Loaded::Objects),
                MetaRequest::Columns(schema, t) => metadata::columns(&s, schema, t).await.map(Loaded::Columns),
                MetaRequest::Indexes(schema, t) => metadata::indexes(&s, schema, t).await.map(Loaded::Indexes),
                MetaRequest::Schemas(db) => metadata::db_schemas(&s, db).await.map(Loaded::Schemas),
            };
            this.update(cx, |m, cx| {
                m.inflight.remove(&req);
                match (res, req) {
                    (Ok(Loaded::Objects(o)), MetaRequest::Objects(schema)) => {
                        m.cache.objects.insert(schema, o);
                    }
                    (Ok(Loaded::Columns(c)), MetaRequest::Columns(schema, t)) => {
                        m.cache.columns.insert((schema, t), c);
                    }
                    (Ok(Loaded::Indexes(i)), MetaRequest::Indexes(schema, t)) => {
                        m.cache.indexes.insert((schema, t), i);
                    }
                    (Ok(Loaded::Schemas(list)), MetaRequest::Schemas(db)) => {
                        let prefix = format!("{db}.");
                        m.cache.schemas.retain(|s| !s.starts_with(&prefix));
                        m.cache.schemas.extend(list);
                        m.cache.schemas.sort();
                        m.cache.loaded_dbs.insert(db);
                    }
                    (Err(e), req) => tracing::warn!("catalog {req:?}: {e}"),
                    _ => unreachable!("response always matches the request"),
                }
                cx.notify();
            })
        })
        .detach();
    }
}
