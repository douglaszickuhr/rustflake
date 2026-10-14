# Notas da API: gpui-kit 0.7.3 / gpui-component 0.7.3 / gpui-pre 0.3.9

Lido do código-fonte publicado no crates.io em 2026-10-14. Referência para as tarefas de
UI do plano. Se o compilador discordar destas notas, o compilador vence: ajuste o código e
atualize esta nota.

## Dependência

Uma só: `gpui-kit = { version = "=0.7.3", features = ["tree-sitter-sql"] }`.

- `use gpui_kit::*;` é o GPUI (div, px, App, Window, Context, Entity, Render, KeyBinding...).
- `gpui_kit::component` = gpui-component; `gpui_kit::assets::Assets` = ícones.
- `gpui_kit::application()` abre a plataforma (macOS). `Application::new()` NÃO existe.
- `gpui_kit::init(cx)` inicializa tema, Root, keymaps de input e tabela.
- `gpui_kit::open_window(options, cx, |window, cx| -> Entity<V>)` já envolve a view em `Root`
  (necessário para dialogs e notificações).
- `gpui_kit::actions!(namespace, [A, B])` — use este macro, não o do gpui.
- `CompletionItem` etc.: crate `lsp-types = "0.97"` (mesma versão do gpui-component).

## Ações e atalhos

- `cx.bind_keys([KeyBinding::new("cmd-enter", RunStatement, Some("SqlTab > Input"))])`.
- Predicado `A > B` = contexto B com A como ancestral.
- O contexto `"Input"` já liga `secondary-enter` (cmd-enter) a inserir nova linha; nossa
  ligação `"SqlTab > Input"` é mais profunda e deve vencer. Verificar na Tarefa 14.
- Na view: `div().key_context("SqlTab").track_focus(&self.focus).on_action(cx.listener(|this, _: &RunStatement, window, cx| ..))`.
- `Root` liga `cmd-c` a copiar texto selecionado; nossa ligação no contexto `"ResultGrid"` vence.

## Editor de código

- `EditorState::new(window, cx).language("sql")` + elemento `Editor::new(&state)`
  (`gpui_kit::component::input::{EditorState, Editor, InputEvent}`). NÃO é `InputState`.
- Texto: `value() -> SharedString`, `text() -> &Rope`, `set_value(v, window, cx)` (não emite
  Change), `replace_all(..)`.
- Cursor em bytes UTF-8: `cursor() -> usize`, `selected_range() -> Range<usize>`.
- Posições LSP: `text().offset_to_position(byte) -> Position` (coluna em chars).
- Eventos: `cx.subscribe_in(&editor, window, |this, _state, ev: &InputEvent, window, cx| ..)`;
  `InputEvent::{Change, PressEnter{secondary, shift}, Focus, Blur}`.
- Autocomplete: `state.lsp_mut().completion_provider = Some(Rc::new(provider))`.
  Trait `CompletionProvider`:
  - `completions(&self, text: &Rope, offset: usize, trigger: lsp_types::CompletionContext, window, cx: &mut App) -> Task<Result<lsp_types::CompletionResponse>>`
  - `is_completion_trigger(&self, offset: usize, new_text: &str, cx: &mut App) -> bool`
  - Itens com `text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text }))`.
- Diagnósticos (sublinhado): `state.diagnostics_mut() -> Option<&mut DiagnosticSet>`;
  `Diagnostic::new(start_pos..end_pos, msg).with_severity(DiagnosticSeverity::Error)`
  (`gpui_kit::component::highlighter::{Diagnostic, DiagnosticSeverity}`).
  **Toda edição de texto limpa o conjunto.**

## Tabela virtualizada

`gpui_kit::component::table::{TableDelegate, TableState, DataTable, Column, ColumnSort, TableEvent, TableSelection}`

- Obrigatórios: `columns_count(&self, cx: &App)`, `rows_count(&self, cx: &App)`,
  `column(&self, col_ix, cx: &App) -> Column`,
  `render_td(&mut self, row_ix, col_ix, window, cx: &mut Context<TableState<Self>>) -> impl IntoElement`.
- Opcionais usados: `has_more(&self, cx: &App) -> bool`, `load_more_threshold(&self) -> usize`,
  `load_more(&mut self, window, cx)` (chamado repetidamente — proteger com flag),
  `perform_sort(&mut self, col_ix, ColumnSort, window, cx)`, `cell_text(&self, row, col, cx: &App) -> String`.
- `TableState::new(delegate, window, cx)`; `.cell_selectable(true)`; `delegate()/delegate_mut()`;
  `refresh(cx)` quando colunas mudam, `cx.notify()` quando só linhas mudam;
  `selection() -> TableSelection::{None, Row(r), Column(c), Cell(r, c)}`.
- Eventos: `TableEvent::{SelectCell(r,c), DoubleClickedCell(r,c), SelectRow(r), ..}`.
- `Column::new(key, name).width(px(..)).sortable()`; `ColumnSort::{Default, Ascending, Descending}`.
- Elemento: `DataTable::new(&state).stripe(true).bordered(true)`.
- **Não existe:** seleção de intervalo de células, copiar.

## Árvore

`gpui_kit::component::tree::{tree, TreeState, TreeItem, TreeEntry, TreeEvent}`, `gpui_kit::component::list::ListItem`

- `TreeState::new(cx)`, `set_items(items, cx)` (zera a seleção), `selected_item()`.
- `TreeItem::new(id, label).children(..).expanded(bool)`; pasta = tem filhos.
- `TreeEvent::{Expanded(SharedString), Collapsed(SharedString)}` — só isso.
- Elemento: `tree(&state, |ix, entry: &TreeEntry, selected, window, cx| ListItem::new(ix).selected(selected).child(..).on_click(..))`.
- `ListItem::on_click(Fn(&ClickEvent, &mut Window, &mut App))`; `ClickEvent::click_count()`.
- **Não existe:** carga preguiçosa (use filho "carregando…"), evento de seleção.

## Outros

- Abas: `TabBar::new("tabs").selected_index(i).on_click(|ix: &usize, window, cx| ..).child(Tab::new().label(..).suffix(..))`.
- Painéis: `h_resizable("id").child(resizable_panel().size(px(260.)).child(..)).child(..)`.
- Barra de status: `gpui_kit::component::status_bar::StatusBar::new().left(..).right(..)`.
- Dialog: `window.open_dialog(cx, |dialog, window, cx| dialog.title(..).child(..).footer(..))`
  (`use gpui_kit::component::WindowExt as _`); crie os `InputState` antes de abrir; feche com
  `window.close_dialog(cx)`.
- Form: `gpui_kit::component::form::{v_form, field}`; `field().label("Host").child(Input::new(&state))`.
- Input simples: `InputState::new(window, cx).placeholder(..)`, senha `.masked(true)`.
- Notificação: `window.push_notification(Notification::error(msg), cx)`.
- Botão: `Button::new(id).label(..).primary().on_click(|ev, window, cx| ..)`
  (`gpui_kit::component::button::{Button, ButtonVariants as _}`).
- Layout: `gpui_kit::component::{h_flex, v_flex, ActiveTheme as _}`; cores em `cx.theme()`.

## Async

- `cx.spawn(async move |this: WeakEntity<T>, cx: &mut AsyncApp| { ..; this.update(cx, |this, cx| ..) })`
  → `Task<R>`; guarde ou `.detach()`.
- `cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| ..)` + `this.update_in(cx, |this, window, cx| ..)`.
- O executor do GPUI não é tokio. Os futures do `rb-core` (`Session::execute` etc.) podem ser
  aguardados direto dentro de `cx.spawn`, porque só usam canais `tokio::sync` (independentes
  de runtime) — o I/O roda no runtime do `Engine`.
