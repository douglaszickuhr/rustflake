use std::time::{Duration, Instant};

use gpui_kit::component::input::CompletionProvider;
use gpui_kit::component::{Rope, RopeExt as _};
use gpui_kit::*;
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, TextEdit};
use rb_core::complete::{CandidateKind, complete};

use crate::meta::MetaStore;

pub struct SqlCompletion {
    meta: Entity<MetaStore>,
}

impl SqlCompletion {
    pub fn new(meta: Entity<MetaStore>) -> Self {
        SqlCompletion { meta }
    }
}

fn kind(k: CandidateKind) -> CompletionItemKind {
    match k {
        CandidateKind::Column => CompletionItemKind::FIELD,
        CandidateKind::Table => CompletionItemKind::CLASS,
        CandidateKind::View => CompletionItemKind::INTERFACE,
        CandidateKind::Function => CompletionItemKind::FUNCTION,
        CandidateKind::Schema => CompletionItemKind::MODULE,
        CandidateKind::Keyword => CompletionItemKind::KEYWORD,
    }
}

/// Itens para o cursor; pede ao MetaStore o que faltar. `true` = faltava catálogo.
fn items(meta: &Entity<MetaStore>, text: &Rope, offset: usize, cx: &mut App) -> (Vec<CompletionItem>, bool) {
    // ponytail: copia o script a cada tecla; ok para scripts de até centenas de KB.
    let src = text.to_string();
    let comp = complete(&src, offset, &meta.read(cx).cache);
    let missing = !comp.missing.is_empty();
    if missing {
        let reqs = comp.missing.clone();
        meta.update(cx, |m, cx| reqs.into_iter().for_each(|r| m.request(r, cx)));
    }
    let range = lsp_types::Range { start: text.offset_to_position(comp.replace.start), end: text.offset_to_position(comp.replace.end) };
    let typed = src[comp.replace.clone()].to_string();
    let items = comp
        .candidates
        .into_iter()
        .map(|c| CompletionItem {
            label: c.label,
            kind: Some(kind(c.kind)),
            detail: c.detail,
            filter_text: Some(typed.clone()),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text: c.insert })),
            ..Default::default()
        })
        .collect();
    (items, missing)
}

/// Quanto o menu espera o catálogo chegar antes de mostrar o que já tem.
const CATALOG_WAIT: Duration = Duration::from_secs(5);

impl CompletionProvider for SqlCompletion {
    fn completions(&self, text: &Rope, offset: usize, ctx: CompletionContext, _: &mut Window, cx: &mut App) -> Task<anyhow::Result<CompletionResponse>> {
        let started = Instant::now();
        let (first, missing) = items(&self.meta, text, offset, cx);
        tracing::debug!(us = started.elapsed().as_micros() as u64, missing, n = first.len(), trigger = ?ctx.trigger_kind, "autocomplete");
        if !missing {
            return Task::ready(Ok(CompletionResponse::Array(first)));
        }
        // Sem o catálogo o menu sairia vazio (ex.: ⌃Espaço logo depois de `from`): espera o
        // MetaStore e refaz; no Snowflake podem ser duas rodadas (schemas, depois objetos).
        let (meta, text) = (self.meta.clone(), text.clone());
        cx.spawn(async move |cx| {
            let mut last = first;
            // Até 3 rodadas: um pedido que falha não vira uma enxurrada de consultas.
            let mut rounds = 0;
            while rounds < 3 && started.elapsed() < CATALOG_WAIT {
                cx.background_executor().timer(Duration::from_millis(50)).await;
                if meta.read_with(cx, |m, _| m.loading()) {
                    continue;
                }
                rounds += 1;
                let (now, missing) = cx.update(|cx| items(&meta, &text, offset, cx));
                last = now;
                if !missing {
                    break;
                }
            }
            Ok(CompletionResponse::Array(last))
        })
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _: &mut App) -> bool {
        !new_text.is_empty() && new_text.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
    }
}
