use std::time::{Duration, Instant};

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::component::input::{CompletionProvider, DefinitionProvider};
use gpui_kit::component::{Rope, RopeExt as _};
use gpui_kit::*;
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, TextEdit};
use rb_core::complete::{CandidateKind, complete, relation_at};

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
        CandidateKind::Database => CompletionItemKind::FOLDER,
        CandidateKind::Keyword => CompletionItemKind::KEYWORD,
    }
}

/// Items for the cursor; asks the MetaStore for whatever is missing. `true` = catalog was missing.
fn items(meta: &Entity<MetaStore>, text: &Rope, offset: usize, cx: &mut App) -> (Vec<CompletionItem>, bool) {
    // ponytail: copies the script on every keystroke; fine for scripts up to hundreds of KB.
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

/// How long the menu waits for the catalog to arrive before showing what it already has.
const CATALOG_WAIT: Duration = Duration::from_secs(5);

impl CompletionProvider for SqlCompletion {
    fn completions(&self, text: &Rope, offset: usize, ctx: CompletionContext, _: &mut Window, cx: &mut App) -> Task<anyhow::Result<CompletionResponse>> {
        let started = Instant::now();
        let (first, missing) = items(&self.meta, text, offset, cx);
        tracing::debug!(us = started.elapsed().as_micros() as u64, missing, n = first.len(), trigger = ?ctx.trigger_kind, "autocomplete");
        if !missing {
            return Task::ready(Ok(CompletionResponse::Array(first)));
        }
        // Without the catalog the menu would come out empty (e.g. ⌃Space right after `from`): wait for the
        // MetaStore and redo; on Snowflake it can take two rounds (schemas, then objects).
        let (meta, text) = (self.meta.clone(), text.clone());
        cx.spawn(async move |cx| {
            let mut last = first;
            // Up to 3 rounds: a failing request does not turn into a flood of queries.
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

/// Table that ⌘-hover found last: (schema as in the cache, name). The click opens this one.
pub type DefinitionTarget = Rc<RefCell<Option<(String, String)>>>;

/// ⌘ + hover underlines the script's table/view; ⌘-click opens its tab (via `show_document`).
pub struct SqlDefinition {
    meta: Entity<MetaStore>,
    target: DefinitionTarget,
}

impl SqlDefinition {
    pub fn new(meta: Entity<MetaStore>, target: DefinitionTarget) -> Self {
        SqlDefinition { meta, target }
    }
}

impl DefinitionProvider for SqlDefinition {
    fn definitions(&self, text: &Rope, offset: usize, _: &mut Window, cx: &mut App) -> Task<anyhow::Result<Vec<lsp_types::LocationLink>>> {
        let Some((range, schema, table)) = relation_at(&text.to_string(), offset, &self.meta.read(cx).cache) else { return Task::ready(Ok(Vec::new())) };
        *self.target.borrow_mut() = Some((schema, table));
        let origin = lsp_types::Range { start: text.offset_to_position(range.start), end: text.offset_to_position(range.end) };
        // The URI only identifies "open table"; which table lives in `target`.
        let Ok(target_uri) = "rustbeaver:table".parse() else { return Task::ready(Ok(Vec::new())) };
        let link = lsp_types::LocationLink {
            origin_selection_range: Some(origin),
            target_uri,
            target_range: Default::default(),
            target_selection_range: Default::default(),
        };
        Task::ready(Ok(vec![link]))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn table_uri_parses() {
        assert!("rustbeaver:table".parse::<lsp_types::Uri>().is_ok());
    }
}
