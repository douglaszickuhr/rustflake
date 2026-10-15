# rustbeaver — busca rápida (⌘K)

Data: 2026-10-15
Status: aprovado em conversa, aguardando revisão da spec

## 1. Objetivo

Achar um database, schema, tabela ou view pelo nome, sem navegar pela árvore, mesmo o que
ainda não foi expandido (no Snowflake da empresa são 87+ databases).

**Sucesso** = ⌘K, digitar parte do nome, Enter: a árvore abre até o objeto e o seleciona; se
for tabela/view, a aba de tabela abre também.

### Escopo

Dentro:
- Popup com campo de busca e até 50 resultados, teclado (↑↓, Enter, Esc) e mouse
- Busca na conexão atual da barra lateral (Postgres e Snowflake)
- Respeita o filtro de databases da conexão (`db_filter`)
- Revelar na árvore (expandir, selecionar, rolar) e abrir a aba de tabela

Fora:
- Busca em colunas, funções, procedures, stages
- Busca em várias conexões ao mesmo tempo
- Histórico de buscas e busca fuzzy (só "contém", sem diferenciar caixa)

## 2. Interação

- Ação `QuickOpen`, atalho `cmd-k` no contexto `Workspace` (vale também dentro do editor
  SQL: nada mais usa `cmd-k`). Sem conexão aberta: notificação "abra uma conexão".
- O popup é um `open_dialog` com uma view própria (`quick_open.rs`): campo em cima, lista
  embaixo, linha selecionada destacada. Abre com o campo focado.
- Snowflake: busca no índice a cada tecla (local, sem espera). Postgres: agenda a query para
  250 ms depois; trocar a `Task` cancela a anterior. Menos de 2 caracteres: lista vazia.
- Linha: ícone do tipo (os mesmos da árvore), nome em destaque, caminho `DB.SCHEMA` em
  cinza, tipo à direita. Status embaixo: "buscando…", "nenhum resultado", erro do banco.
- Enter / clique: fecha o popup e revela o objeto (seção 5).

## 3. Busca (`rb-core`, módulo `quick_open`)

```rust
pub enum HitKind { Database, Schema, Table, View }
/// `matview` só vale com `View`: decide o grupo da árvore (`views` ou `matviews`).
pub struct Hit { pub kind: HitKind, pub matview: bool, pub database: Option<String>, pub schema: Option<String>, pub name: String }
pub async fn pg_search(s: &Session, text: &str) -> Result<Vec<Hit>, DbError>   // Postgres, ao vivo
```

### Snowflake: índice em memória

A busca não vai ao banco: procura num índice da conexão, mantido por um indexador.

```rust
// rb-core/src/quick_open.rs
pub struct SfIndex { pub objects: Vec<Hit>, pub schemas: Vec<Hit>, pub refreshed: Option<Instant> }
pub async fn sf_snapshot(s: &Session) -> Result<SfIndex, DbError>   // lista tudo
pub struct IndexDiff { pub changed_schemas: BTreeSet<String>, pub changed_dbs: BTreeSet<String> }  // `DB.SCHEMA` / `DB`
pub fn diff(old: &SfIndex, new: &SfIndex) -> IndexDiff              // entrou, saiu ou mudou de tipo
pub fn search_index(idx: &SfIndex, cache: &MetaCache, text: &str) -> Vec<Hit>  // rank + db_filter
```

- `sf_snapshot`: `SHOW TERSE SCHEMAS IN ACCOUNT` e `SHOW TERSE OBJECTS IN ACCOUNT`.
  O `SHOW` devolve no máximo 10.000 linhas: se a lista de objetos vier com exatamente 10.000,
  refaz por database (`SHOW TERSE OBJECTS IN DATABASE "X"`, só nas `visible_databases()`).
  Uma database com mais de 10.000 objetos fica truncada (`tracing::warn`).
  <!-- ponytail: sem paginação `FROM`; adicionar se uma database passar de 10k objetos -->
- Databases vêm de `cache.databases` (já carregadas pela árvore), não do índice.
- O indexador usa **uma sessão própria** (`Engine::connect` da mesma conexão: reaproveita o
  login, sem novo SSO), para não ficar na fila das expansões da árvore e do autocomplete.
  `SHOW … IN ACCOUNT` não depende do `USE` da sessão.

#### Ciclo (`rb-app/src/indexer.rs`)

- Começa ao conectar (Snowflake). Primeira carga: o popup mostra "indexando…" e busca vazia
  até ela terminar.
- **A cada 30 s** refaz o `sf_snapshot`, calcula `diff` com o índice atual e troca o índice.
  Para cada schema em `changed_schemas` que a árvore já carregou (`cache.objects` tem a chave):
  `invalidate_schema` + `request(Objects)`; para cada database em `changed_dbs` (schema novo ou
  removido) já carregada: `request(Schemas)`. A árvore mostra a mudança sozinha.
- Não sobrepõe: se um snapshot ainda está rodando, o tick seguinte é pulado.
- Erro no snapshot: mantém o índice anterior, `tracing::warn`, tenta no próximo tick.
- Para ao desconectar (a `Task` cai junto com o `Open`).
- Botão ↻ no popup: snapshot imediato.

### Postgres

Uma query com `query_text` e parâmetro `$1 = like_pattern(texto)`:

```sql
select 'schema', nspname, null from pg_namespace
 where nspname ilike $1 and nspname not like 'pg\_%' and nspname <> 'information_schema'
union all
select c.relkind::text, n.nspname, c.relname from pg_class c join pg_namespace n on n.oid = c.relnamespace
 where c.relkind in ('r','p','v','m','f') and c.relname ilike $1
   and n.nspname not like 'pg\_%' and n.nspname <> 'information_schema'
limit 200
```

`r`, `p`, `f` → `Table`; `v` → `View`; `m` → `View` com `matview = true`. `database` fica `None`.

### Postgres: busca ao vivo

Sem índice: a query acima é local ao catálogo e responde em milissegundos.

### Filtro e ordem (funções puras, testadas)

- `like_pattern(text) -> String`: `%texto%` com `%`, `_` e `\` escapados por `\` (vai como
  parâmetro `$1`, então aspas não precisam de escape).
- `rank(hits, text) -> Vec<Hit>`: mantém só quem contém o texto (sem diferenciar caixa),
  ordena por exato < prefixo < contém, depois nome mais curto, depois alfabético; corta em 50.

## 4. Do resultado ao nó da árvore

`Hit::tree_path() -> Vec<Vec<String>>` devolve as partes de cada id (formato de `schema_tree.rs`;
o app junta com `\u{1f}`) do nó raiz até o alvo:

| Alvo | Snowflake | Postgres |
|---|---|---|
| Database | `d·DB` | — |
| Schema | `d·DB`, `s·DB.S` | `s·S` |
| Tabela | `d·DB`, `s·DB.S`, `g·DB.S·tables`, `r·DB.S·T` | `s·S`, `g·S·tables`, `r·S·T` |
| View | … `g·…·views` (matview: `matviews`) … | idem |

## 5. Revelar na árvore

`SchemaTree::reveal(path: Vec<SharedString>, cx)`:
1. Põe todos os ids, menos o último, em `expanded` e pede ao `MetaStore` o que falta
   (`Schemas(db)`, `Objects(schema)`), igual ao `TreeEvent::Expanded`.
2. Guarda `pending_reveal = Some(alvo)` e reconstrói.
3. A cada `rebuild`, se o alvo já existe nos itens (`TreeState::index_of`): `reveal_item`
   (seleciona e rola) e limpa `pending_reveal`. Os níveis que ainda carregam mostram
   "carregando…" e o alvo aparece quando o metadado chega.
4. Se o carregamento falhar, `pending_reveal` fica e não faz nada (o erro já aparece na árvore).
   Um novo `reveal` substitui o anterior.

Tabela/view: além de revelar, `Workspace::open_table(conn, schema_key, name)` (o mesmo do
duplo clique).

A barra lateral recolhida (`collapsed`) é expandida antes de revelar.

## 6. Testes

- `rb-core`:
  - `like_pattern`: `%`, `_`, `\`.
  - `rank`: exato antes de prefixo antes de contém, desempate por tamanho, descarte do que não contém.
  - Parse das linhas de `SHOW TERSE OBJECTS/SCHEMAS` (colunas por nome, como em `snowflake.rs`).
  - `diff`: tabela nova, removida, view virando tabela; schema novo; nada mudou → vazio.
  - `search_index` respeita `db_filter`.
  - `tree_path` nos dois dialetos, incluindo matview.
- `rb-app`: `reveal` com cache já carregado seleciona o nó; com cache vazio deixa
  `pending_reveal` e conclui depois de preencher o cache (teste de `build_items` + lógica pura).
- Manual: criar uma tabela no Snowsight e vê-la no ⌘K (e na árvore, se o schema estiver
  aberto) em até 30 s; ⌘K no Snowflake real com `DEV_`, com filtro de databases ativo, e numa tabela de
  database ainda não expandido.
