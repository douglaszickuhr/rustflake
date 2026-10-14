use std::time::Instant;

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

impl CompletionProvider for SqlCompletion {
    fn completions(&self, text: &Rope, offset: usize, _: CompletionContext, _: &mut Window, cx: &mut App) -> Task<anyhow::Result<CompletionResponse>> {
        let started = Instant::now();
        // ponytail: copia o script a cada tecla; ok para scripts de até centenas de KB.
        let src = text.to_string();
        let comp = complete(&src, offset, &self.meta.read(cx).cache);
        if !comp.missing.is_empty() {
            // Carrega em segundo plano; os itens aparecem na próxima tecla.
            let missing = comp.missing.clone();
            self.meta.update(cx, |m, cx| missing.into_iter().for_each(|r| m.request(r, cx)));
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
        tracing::debug!(us = started.elapsed().as_micros() as u64, "autocomplete");
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _: &mut App) -> bool {
        !new_text.is_empty() && new_text.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
    }
}
