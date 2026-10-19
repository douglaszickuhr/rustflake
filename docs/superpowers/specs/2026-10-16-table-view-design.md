# Visão de tabela: Definição, Linhagem e dbt (Snowflake)

Referência visual: canvas "Rustflake UX Concepts", boards 7–10
(https://claude.ai/artifact/KeTaipPVZnaCRzi68f6V5p): `TableDefinition`, `TableLineage`, `TableDbt`, `DbtSetup`.

## Objetivo

Ao abrir uma tabela/view do Snowflake, responder:

- **de onde vem**: linhagem a montante até as fontes;
- **o que é**: modelo dbt, com SQL, docs, testes, owner, link para o GitHub;
- **detalhes de cada coluna**: tipo, descrição, classificação C2–C4, mascaramento, linhagem da coluna.

## Fora do escopo

- Impacto a jusante como tela própria. O jusante aparece só no painel do nó, vindo do dbt.
- Postgres.
- Editar qualquer coisa: modelo dbt, comentários, tags.
- Resultados de teste vindos de produção. Só o `run_results.json` local.

## Decisões do usuário

| Pergunta | Escolha |
|---|---|
| Fonte do dbt | Clone local + `manifest.json` |
| Casar tabela ↔ modelo | `dbt parse --target <alvo>` (relation_name exato) |
| Profundidade | Até as fontes |
| Lugar | Seções na aba de tabela |
| Resultado de teste | `target/run_results.json` local |
| Rótulo das camadas | Primeira pasta em `models/` |
| Jusante | `child_map` do manifest |
| Diálogo de conexão | Abas General / Authentication / dbt |
| Entrega | 2 planos: (1) dbt + Definição + dbt; (2) Linhagem |

## 1. Configuração do dbt (board 10)

**Config:** `SnowflakeConfig.dbt: Option<DbtConfig { project_dir: String, target: String }>`, com serde default e omitido quando vazio.

**Diálogo de conexão** passa a ter abas:

- **General**: nome, ambiente, conta/host, database, schema, warehouse, role.
- **Authentication**: usuário, método, chaves, senha, PAT, SSH.
- **dbt**: só Snowflake.
  - **Pasta do projeto**: campo + "Escolher…" com o seletor de pasta do macOS.
  - **Target**: campo de texto, padrão `prod`.
  - **Cartão de estado**:
    - "Manifest encontrado · gerado há 3 h", com o botão **Refresh dbt**;
    - contagens: modelos, sources, seeds e "N casam com tabelas daqui";
    - GitHub: `github.com/org/repo · branch main`.
  - Nota: "Refresh roda `dbt parse --target <t>` na pasta; só lê arquivos, não consulta o Snowflake".

**Refresh dbt** roda `dbt parse --target <t>` com `std::process::Command` dentro da pasta do projeto.

- Timeout de 120 s.
- Falhou: cartão vermelho com a saída (stdout+stderr), botões "Copiar saída" e "Tentar de novo", e uma dica sobre `~/.dbt/profiles.yml`.
- `dbt` fora do PATH: mensagem "dbt não encontrado no PATH".

**Leitura do dbt** (rb-core `dbt.rs`, puro e testável):

- Lê `<dir>/target/manifest.json` com serde, só os campos usados:
  - `nodes` (`model`, `seed`, `snapshot`) e `sources`;
  - `parent_map` e `child_map`;
  - de cada nó: `name`, `resource_type`, `relation_name`, `original_file_path`, `fqn`, `description`, `columns{name: {description}}`, `config{materialized, incremental_strategy, unique_key, tags, meta}`, `meta`, `raw_code`, `compiled_code` (opcional) e `depends_on.nodes`;
  - os nós de teste: `test_metadata{name, kwargs}`, `attached_node` e `column_name`.
- Índice `relation_name` normalizado (sem aspas, maiúsculas, `DB.SCHEMA.NOME`) → `unique_id`.
- Owner: `meta.owner`, ou `config.meta.owner`, ou o grupo do modelo.
- Camada: `fqn[1]`, a primeira pasta sob `models/`, com a primeira letra maiúscula. Sources e seeds viram "Sources".
- `run_results.json` é opcional: `unique_id` → (`status`, `timing.completed_at`). Só vale se veio do mesmo target, comparando `metadata.env` / `args.target`; se não der para confirmar, mostra sem status.
- Git:
  - `git -C dir remote get-url origin`, normalizando SSH e HTTPS para `https://github.com/org/repo`;
  - `git rev-parse --abbrev-ref HEAD` e `git rev-parse --short HEAD`;
  - URL do arquivo: `<repo>/blob/<branch>/<original_file_path>`.
- Carregado uma vez por conexão, em segundo plano, e recarregado depois de um Refresh. O mtime do manifest dá o "gerado há".

## 2. Aba de tabela com seções (boards 7–9)

A aba de tabela atual ganha uma faixa de cabeçalho e as seções **Data | Definition | Lineage | dbt**.

- **Data** é a grade de hoje (LIMIT 1000), sem mudança.
- Lineage entra no plano 2. Até lá, a seção não aparece.

**Cabeçalho** (todas as seções):

- nome, `DB.SCHEMA`;
- selo **dbt model · incremental**, quando casa;
- owner e tags em chips;
- botão **Query**, que abre uma aba SQL com `select * from DB.SCHEMA.T limit 100`;
- linha de fatos: tipo (Table/View/Dynamic table), linhas, tamanho, "alterada há", comentário.
- Fonte: `INFORMATION_SCHEMA.TABLES` do database da tabela (`row_count`, `bytes`, `last_altered`, `comment`, `table_type`).

**Definição** (board 7):

- Linha de resumo: `N colunas · N classificadas · N testes · N sem descrição` (a última em laranja).
- Tabela com:

  | Coluna | Conteúdo |
  |---|---|
  | Coluna | nome |
  | Tipo | tipo |
  | Descrição | a do dbt, senão o comentário do Snowflake, com a origem marcada (`dbt` / `comment`); sem nenhuma, "Sem descrição" em laranja |
  | Classificação | chip C1–C4 da tag `GOVERNANCE.TAGS.DATA_CLASSIFICATION`, com cadeado se houver masking policy |
  | Testes dbt | chips dos testes da coluna |

- Fontes:
  - colunas: `DESCRIBE TABLE` (já existe) + `INFORMATION_SCHEMA.COLUMNS`, para comentário e default;
  - tags: `table(<db>.information_schema.tag_references_all_columns('<db.s.t>', 'table'))`;
  - masking: `table(<db>.information_schema.policy_references(ref_entity_name => '<db.s.t>', ref_entity_domain => 'table'))`.
- **Painel da coluna selecionada**:
  - tipo · nullable · default;
  - Descrição (dbt e comentário do Snowflake, quando os dois existem);
  - Proteção (nível e tag, policy);
  - **Vem de**: `GET_LINEAGE('<db.s.t.col>', 'COLUMN', 'UPSTREAM', 5)` em cadeia, cada nó com o ponto da cor do tipo (dbt model / source / seed / fora do dbt) e "linhagem do Snowflake · até ~3 h de atraso". Carrega ao selecionar.
  - Testes com status ("passou na última execução · há 3 h"), quando houver `run_results`.

**dbt** (board 9):

- **Esquerda**:
  - descrição;
  - materialização + estratégia, unique key, owner, meta (pares chave/valor);
  - tags;
  - **Testes · N** agrupados por tipo, com as colunas: `not_null USAGE_DATE, ACCOUNT_ID…`, `relationships ACCOUNT_ID → stg_x`, `accepted_values SERVICE · 6 valores`; o status vem quando houver;
  - rodapé "Manifest · target prod · gerado há 3 h" + **Refresh dbt**.
- **Direita**:
  - caminho do arquivo e alternância **Fonte / Compilado**;
  - **Abrir no GitHub** (abre o navegador) e **Abrir no editor** (`open <dir>/<arquivo>`);
  - SQL somente leitura com destaque no `Editor` existente; o Jinja também ganha destaque, porque o highlighter de SQL o trata como texto;
  - "Compilado" fica desabilitado se o manifest não tiver `compiled_code`, com a dica "rode dbt compile";
  - rodapé `main @ 4f2a91c`.
- **Estados**, todos do board 10:
  - dbt não configurado: "Configurar dbt…" abre o diálogo da conexão na aba dbt;
  - sem manifest: Refresh;
  - tabela sem modelo correspondente: "Esta tabela não é um modelo dbt deste projeto".

## 3. Linhagem (plano 2, board 8)

- **Montante até as fontes**: `GET_LINEAGE('<db.s.t>', 'TABLE', 'UPSTREAM', 5)`.
  - Remove as arestas de uma tabela para ela mesma (MERGE incremental).
  - Junta com o grafo do dbt (`parent_map`). As arestas que só o dbt conhece ficam tracejadas: "conhecida só pelo dbt (sem leitura no ACCESS_HISTORY ainda)".
- Alternância **Snowflake / grafo dbt**.
  - Se `GET_LINEAGE` falhar (edição sem Enterprise ou sem VIEW LINEAGE), cai no grafo dbt com o aviso do board 10.
- **Layout em camadas, da esquerda para a direita**:
  - colunas pela camada (pasta do dbt), "Sources" à esquerda e "Esta tabela" à direita;
  - se uma pasta aparecer em mais de uma distância, a coluna vale pela distância;
  - layout puro em rb-core, testável; zoom e ⌘+scroll como no `PlanView`.
- **Nós**:
  - ponto e rótulo do tipo: dbt model · materialização / dbt source · nome / dbt seed / fora do dbt · ingestão, com borda tracejada;
  - nome e `DB.SCHEMA`;
  - legenda no topo, e "Montante · N níveis · de há X".
- **Painel do nó selecionado**:
  - tipo, nome, local, descrição (dbt);
  - linhas e alterada (`INFORMATION_SCHEMA.TABLES`, ao selecionar);
  - owner;
  - Montante, um resumo por tipo;
  - Jusante, do `child_map`;
  - botões **Abrir tabela** e **Modelo dbt**, que abrem a aba da tabela na seção dbt.

## Erros

- Cada seção e cada painel falham sozinhos.
- Dicas:
  - IMPORTED PRIVILEGES / VIEW LINEAGE para a linhagem;
  - permissão no database para `INFORMATION_SCHEMA`;
  - saída do dbt quando o parse falha.
- `GET_LINEAGE` vazio (objeto recém-criado): "Ainda sem linhagem no Snowflake", e o grafo dbt.

## Testes (rb-core)

- `dbt.rs`:
  - manifest reduzido em fixture: índice relation_name→nó com aspas e caixa, owner, camada pelo `fqn`, testes por coluna (not_null, unique, accepted_values, relationships) e agrupados por tipo, `child_map`;
  - `run_results`: status por teste e checagem do target;
  - URL do GitHub a partir de `git@github.com:org/repo.git`, `https://github.com/org/repo(.git)` e `ssh://…`;
  - nada de manifest: erro claro.
- Definição: junção das colunas (Snowflake + dbt + tags + policies), a origem da descrição e o resumo.
- Linhagem (plano 2):
  - parse do `GET_LINEAGE` (fixture no formato real visto na conta), sem autoarestas;
  - junção com o dbt (arestas tracejadas);
  - camadas por pasta, com fallback por distância;
  - layout sem sobreposição.
- A UI é verificada à mão:
  - abrir uma tabela de mart;
  - as três seções;
  - Refresh;
  - Abrir no GitHub e no editor;
  - os estados sem dbt e sem manifest.
