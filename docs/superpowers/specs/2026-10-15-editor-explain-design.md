# rustbeaver — editor (formatar, histórico, várias abas de resultado) e EXPLAIN visual

Data: 2026-10-15
Status: aprovado em conversa, aguardando revisão da spec
Subprojetos 3 e 4 de 5, implementados juntos (o 4 usa a barra de abas de resultado do 3).

## 1. Objetivo

No editor SQL: formatar o SQL, reencontrar queries já executadas e ver o resultado de cada
statement de um script. E entender por que uma query é lenta, com o plano desenhado como
diagrama, no Postgres e no Snowflake.

**Sucesso**
- ⌘⇧F deixa o SQL selecionado (ou o script) legível, com palavras-chave em MAIÚSCULAS; ⌘Z desfaz.
- ⌘Y, digitar parte de uma query de ontem, Enter: ela está no editor.
- ⌘⇧Enter num script com 3 SELECTs e 1 UPDATE mostra 3 abas de resultado e uma de mensagens.
- ⌘⇧E numa query lenta mostra o diagrama com o nó mais caro em vermelho e selecionado, e
  um UPDATE analisado não altera nada no banco.

### Fora do escopo

- Configurar o estilo do formatador (fixo: MAIÚSCULAS, 2 espaços).
- Histórico compartilhado entre conexões, sincronizado ou exportável.
- Fixar abas de resultado; paginação além de 1000 linhas nas abas que não são a última.
- Comparar dois planos; EXPLAIN de DDL; editar o plano; exportar imagem.
- Árvore textual do plano (o plano é sempre diagrama).

## 2. Atalhos

| Atalho | Contexto | Ação |
|---|---|---|
| ⌘⇧F | `SqlTab > Input` | Formatar seleção ou script |
| ⌘⌥F | `SqlTab > Input` | Localizar e substituir (era ⌘⇧F no editor do gpui-kit) |
| ⌘Y | `Workspace` | Popup do histórico da conexão atual |
| ⌘E | `SqlTab > Input` | Plano estimado do statement sob o cursor |
| ⌘⇧E | `SqlTab > Input` | Plano real (executa, com rollback) |
| ⌘+ / ⌘− / ⌘0 | `PlanView` | Zoom do diagrama / ajustar à tela |

Os atalhos em `SqlTab > Input` vencem os do editor (`Input`), como já acontece com ⌘. e Tab.

## 3. Formatar SQL

- `rb-core/src/format.rs`: `pub fn format_sql(sql: &str) -> String`, sobre o crate
  `sqlformat = "0.5"` com `uppercase: Some(true)`, `indent: Indent::Spaces(2)`,
  `lines_between_queries: 1`, sem parâmetros (`QueryParams::None`).
- No `SqlTab`: com seleção, formata só ela; sem seleção, seleciona tudo e formata. A troca usa
  `EditorState::replace` (uma edição atômica: um ⌘Z desfaz).
- Texto igual depois de formatar: nada muda (não suja o desfazer).
- Precisam sair intactos: `$1`, `:nome`, `?`, strings `'…'` (inclusive com `--` dentro),
  identificadores `"…"`, comentários `--` e `/* */`, corpos `$$ … $$`. Cada um com teste;
  se o `sqlformat` estragar algum, o caso entra como teste que documenta o limite e o formatador
  pula o statement (devolve o original daquele statement).

## 4. Histórico

### Armazenamento (`rb-core/src/history.rs`)

```rust
pub struct Entry { pub ts: i64 /* unix ms */, pub sql: String, pub ms: u64, pub rows: Option<u64>, pub error: Option<String> }
pub fn history_path(conn_id: &str) -> PathBuf          // app_dir()/history/<id>.jsonl
pub fn append(path: &Path, e: &Entry) -> Result<(), DbError>
pub fn load(path: &Path) -> Vec<Entry>                 // mais novo primeiro
pub fn search(entries: &[Entry], text: &str) -> Vec<&Entry>
pub const MAX_ENTRIES: usize = 5_000;
```

- JSON Lines (`serde_json`, já é dependência): uma entrada por linha; quebras de linha do SQL
  ficam escapadas pelo JSON.
- Arquivo criado com permissão `0600` (pode conter senhas de `ALTER USER … PASSWORD`).
- `load` ignora linhas inválidas (`tracing::warn`) e, se houver mais de `MAX_ENTRIES`, reescreve
  o arquivo (atômico) só com as mais recentes.
- `search`: "contém" sem diferenciar caixa; texto vazio = tudo. Entradas seguidas com o mesmo SQL
  (após `trim`) aparecem uma vez só, com a data da mais recente.
- Grava cada statement executado pelo `SqlTab` (⌘Enter e ⌘⇧Enter), com sucesso ou erro, em
  segundo plano. Não grava: consultas internas (árvore, autocomplete, metadados, ⌘K, aba de
  tabela, aba de usuários), EXPLAIN, statements cancelados antes de começar.

### Popup (`rb-app/src/history_popup.rs`)

- No molde do `quick_open.rs`: `open_dialog` com campo de busca focado e lista.
- Título "Histórico — <conexão>". Sem conexão atual: notificação "abra uma conexão".
- Linha: primeira linha do SQL (cortada com "…"), embaixo "hoje 21:14 · 12 ms · 1 linha"
  (ou "erro" em vermelho). Até 200 resultados.
- ↑↓ movem, Enter insere no cursor da aba SQL ativa se ela é desta conexão; senão abre um
  script novo desta conexão com o texto. ⌘Enter sempre abre script novo. Esc fecha.
- A lista é carregada ao abrir o popup (arquivo local, em segundo plano).

## 5. Várias abas de resultado

- O `SqlTab` passa a ter `results: Vec<ResultPane>`, `messages: Vec<Message>`, `plan: Option<Entity<PlanView>>`
  e `active: Pane` (`Result(i)`, `Messages`, `Plan`).
  - `ResultPane { title, table: Entity<TableState<ResultGrid>> }`.
  - `Message { sql_head: String, rows: Option<u64>, affected: Option<u64>, ms: u64, error: Option<String> }`.
- **⌘Enter** (um statement): uma `ResultPane`, sem barra de abas (como hoje).
- **⌘⇧Enter**: uma `ResultPane` "Resultado N" por statement que devolve colunas; se o script tem
  mais de um statement, a aba "Mensagens" lista todos. A aba ativa no fim é o último resultado.
- Rodar de novo (qualquer um dos dois) substitui resultados, mensagens e plano.
- Erro no statement k: para ali; abas e mensagens até k ficam, a mensagem k tem o erro; marcação
  no editor e notificação continuam como hoje; a aba ativa vira "Mensagens" se houver.
- Paginação: só a última `ResultPane` tem o portal aberto (a sessão mantém um cursor). As
  anteriores ficam com `more = false` e mostram "N+ linha(s) (só as primeiras)".
- O skeleton e o spinner (já existentes) valem para a aba de resultado ativa.
- A distribuição statements → abas/mensagens é uma função pura testável:
  `fn panes(outcomes: &[Outcome]) -> (Vec<usize> /* índices com colunas */, bool /* mostra Mensagens */)`.

## 6. EXPLAIN

### Comandos

- ⌘E: estimado; ⌘⇧E: real. Statement sob o cursor (`statement_at`).
- Aceitos: começa (após comentários/espaços) com `SELECT`, `WITH`, `INSERT`, `UPDATE`, `DELETE`,
  `MERGE`, `VALUES` ou `TABLE`. Outro → notificação "EXPLAIN só para consultas e DML", nada executa.
- O resultado vai para a aba "Plano"; os resultados e mensagens anteriores continuam nas suas abas.

### Modelo (`rb-core/src/explain.rs`)

```rust
pub struct PlanNode {
    pub op: String,                    // "Hash Join", "TableScan"
    pub object: Option<String>,        // tabela/índice
    pub detail: Vec<(String, String)>, // filtro, join cond, buffers, partições, spill…
    pub est_rows: Option<f64>,
    pub act_rows: Option<f64>,
    pub cost: Option<f64>,             // exclusivo
    pub time_ms: Option<f64>,          // exclusivo
    pub pct: f64,                      // 0..=100, peso do nó para cor/destaque
    pub warn: Vec<String>,
    pub children: Vec<PlanNode>,
}
pub struct Plan { pub root: PlanNode, pub analyzed: bool, pub total_ms: Option<f64> }
pub fn parse_pg(json: &str, analyzed: bool) -> Result<Plan, DbError>
pub fn parse_sf_explain(json: &str) -> Result<Plan, DbError>
pub fn parse_sf_stats(rows: &[Vec<Option<String>>]) -> Result<Plan, DbError>
```

### Postgres

- Estimado: `EXPLAIN (FORMAT JSON) <sql>`.
- Real: `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) <sql>` dentro de uma transação que termina
  **sempre** em ROLLBACK (mesmo SELECT: funções com efeito colateral). Novo comando no ator
  (`Session::explain(sql, analyze) -> Result<String, DbError>`), que não passa pelo bloqueio de
  BEGIN do usuário.
- Exclusivo: `actual total time × loops` menos o dos filhos (real); `total cost` menos o dos
  filhos (estimado). `pct` = exclusivo / soma dos exclusivos.
- Detalhes: `Filter`, `Index Cond`, `Hash Cond`, `Join Filter`, `Rows Removed by Filter`,
  `Sort Key`, buffers `shared hit/read`, `Workers`.
- Avisos: `max(est, act) / max(min(est, act), 1) > 10` (real); Seq Scan com
  `Rows Removed by Filter > 9 × linhas devolvidas`.

### Snowflake

- Estimado: `EXPLAIN USING JSON <sql>` (só compila, não liga warehouse). O JSON tem
  `GlobalStats` e `Operations` (lista de operadores com `id`, `parent`/`parentOperators`,
  `operation`, `objects`, `expressions`, `partitionsAssigned`, `partitionsTotal`, `bytesAssigned`).
  `pct` dos scans = bytes do scan / bytes totais; demais 0.
- Real: no ator Snowflake, `BEGIN`, a query (resultado descartado), guarda o `query_id` do
  cursor, `SELECT * FROM TABLE(GET_QUERY_OPERATOR_STATS('<query_id>'))`, `ROLLBACK`.
  `pct` = `EXECUTION_TIME_BREAKDOWN:overall_percentage`; linhas = `OPERATOR_STATISTICS:output_rows`;
  árvore por `OPERATOR_ID`/`PARENT_OPERATORS`.
- Avisos: `bytes_spilled_remote_storage > 0`; scan com `partitions_scanned / partitions_total > 0.9`
  e `partitions_total > 100` (poda ruim).
- Um operador com vários pais (raro): fica sob o primeiro pai.

### Diagrama (`rb-app/src/plan_view.rs`)

- Layout em `rb-core/src/explain.rs` (testável):
  `pub fn layout(root: &PlanNode, box_w: f32, box_h: f32, gap_x: f32, gap_y: f32) -> Vec<Placed>`
  com `Placed { path: Vec<usize>, x, y, parent: Option<usize> }`. A largura de uma folha é
  `box_w`; a de um nó, a soma das larguras dos filhos mais os `gap_x`; o pai fica centralizado
  sobre o intervalo dos filhos; profundidade d → `y = d × (box_h + gap_y)`.
- Caixas (`div` absolutas, 220×64 no zoom 1): operação + %, objeto, "tempo ou custo · linhas", ⚠.
  Fundo misturando o fundo do tema com vermelho na proporção `pct` (0% neutro, ≥50% vermelho
  forte); funciona no claro e no escuro.
- Setas filho → pai num `canvas` com `PathBuilder`; espessura `1 + log10(linhas + 1)` px
  (máx. 8), rótulo com as linhas (`1.2M`, `98k`).
- Área rolável nos dois eixos; zoom 0.25–2.0 (⌘+, ⌘−); ⌘0 e a abertura ajustam à tela.
- Painel à direita (280 px) com todos os detalhes do nó selecionado; começa no nó de maior `pct`.
- Cabeçalho: "Plano estimado" ou "Plano real · 1.2 s".

## 7. i18n

Textos novos em `rb-app/locales/app.toml` (pt/en/es): `editor.format`, `history.*`
(título, vazio, erro, hoje/ontem, linhas), `results.*` (Resultado N, Mensagens, só as primeiras),
`plan.*` (estimado, real, só consultas e DML, zoom). Mensagens de erro do core em
`rb-core/locales/app.toml` (`err.explain_*`).

## 8. Erros

- Falha ao gravar histórico: `tracing::warn`, nunca interrompe a execução.
- JSON de plano inesperado: notificação com o erro de parse; a aba "Plano" não abre.
- Falha no ROLLBACK do real: erro na notificação e na sessão marcada para reconectar (o ator
  descarta a conexão), para não deixar transação aberta.
- Cancelar (⌘.) vale também durante o EXPLAIN real.

## 9. Testes

- `format`: maiúsculas e quebras; cada caso de preservação da seção 3; texto já formatado não muda.
- `history`: append/load com SQL de várias linhas; ordem; linha corrompida ignorada; corte em
  `MAX_ENTRIES` reescreve o arquivo; permissão 0600; `search` com caixa e repetições seguidas.
- `panes`: só SELECTs, mistura com DML, um statement só, erro no meio.
- `explain`: fixtures JSON reais em `crates/rb-core/tests/fixtures/` (Postgres estimado e real
  com join aninhado; Snowflake EXPLAIN e GET_QUERY_OPERATOR_STATS); tempo exclusivo com loops;
  avisos; `layout` sem sobreposição, pai centralizado, folhas em sequência.
- Integração Postgres (`RB_TEST_DATABASE_URL`): plano real de `UPDATE` não altera a tabela;
  plano estimado de SELECT devolve JSON que `parse_pg` aceita; DDL é recusado.
- Snowflake real: só nos testes de integração existentes com `RB_TEST_SNOWFLAKE_*` (não rodam
  sem a conta).
