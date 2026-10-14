use std::collections::HashSet;
use std::sync::Arc;

use gpui_kit::*;
use rb_core::config::ConnectionConfig;
use rb_core::metadata::{self, ColumnMeta, IndexMeta, MetaCache, MetaRequest, SchemaObjects};
use rb_core::session::{Credentials, Session};

#[derive(Clone)]
pub struct ConnTarget {
    pub cfg: ConnectionConfig,
    pub creds: Credentials,
}

/// Sessão dedicada a metadados + cache compartilhado por árvore e autocomplete.
pub struct MetaStore {
    pub session: Arc<Session>,
    pub cache: MetaCache,
    pub error: Option<String>,
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
        let mut this = MetaStore { session, cache: MetaCache::default(), error: None, inflight: HashSet::new() };
        this.reload_roots(cx);
        this
    }

    /// Recarrega schemas e search_path (raiz da árvore).
    pub fn reload_roots(&mut self, cx: &mut Context<Self>) {
        let s = self.session.clone();
        cx.spawn(async move |this, cx| {
            let res = async { Ok::<_, rb_core::DbError>((metadata::schemas(&s).await?, metadata::search_path(&s).await?)) }.await;
            this.update(cx, |m, cx| {
                match res {
                    Ok((schemas, path)) => {
                        m.cache.schemas = schemas;
                        m.cache.search_path = path;
                    }
                    Err(e) => m.error = Some(e.to_string()),
                }
                cx.notify();
            })
        })
        .detach();
    }

    /// Carrega um pedaço do catálogo se ainda não estiver em cache nem a caminho.
    pub fn request(&mut self, req: MetaRequest, cx: &mut Context<Self>) {
        if self.cache.has(&req) || !self.inflight.insert(req.clone()) {
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
                    (Err(e), _) => m.error = Some(e.to_string()),
                    _ => unreachable!("resposta sempre corresponde ao pedido"),
                }
                cx.notify();
            })
        })
        .detach();
    }
}
