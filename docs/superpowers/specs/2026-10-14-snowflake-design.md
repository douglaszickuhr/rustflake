# rustbeaver — conexões Snowflake

Data: 2026-10-14
Status: aprovado em conversa, aguardando revisão da spec
Parte do subprojeto 5 ("outros bancos"), antecipado pelo autor. Depende do branch `feat/ssh-tunnel`.

## 1. Objetivo

Usar o rustbeaver também com as contas Snowflake do autor: escrever e rodar SQL, navegar nos
objetos e ver dados de tabelas, trocando warehouse e role sem sair do app.

**Sucesso** = o autor conecta por SSO no navegador, key-pair ou PAT; roda queries com resultado
paginado e cancelável; navega database → schema → objetos → colunas; abre tabelas na grade
(somente leitura); troca warehouse/role por um seletor.

### Escopo

Dentro:
- Autenticação: SSO no navegador (`externalbrowser`), key-pair (`.p8`, com ou sem passphrase), PAT
- Editor SQL com execução paginada, cancelamento e autocomplete
- Árvore: databases → schemas → tabelas / views / views materializadas / funções → colunas
- Abrir tabela na grade com filtro e ordenação, somente leitura
- Seletor de warehouse e role da conexão ativa

Fora:
- Editar dados na grade do Snowflake
- Autenticação por senha (com ou sem MFA) e OAuth com fluxo próprio
- Autocomplete com nome em três partes (`DB.SCHEMA.TABELA`)
- Comentários `//` na divisão de statements
- Stages, tasks, streams, pipes, dynamic/external tables como nós próprios da árvore
- Transações explícitas (`BEGIN`)

## 2. Modelo e conexão

`ConnectionConfig` ganha `snowflake: Option<SnowflakeConfig>`
(`#[serde(default, skip_serializing_if = "Option::is_none")]`). Com o bloco presente, a conexão é
Snowflake; `host`, `port`, `ssl` e `ssh` são ignorados (e passam a ter `#[serde(default)]`).
`database` (opcional no Snowflake; vazio = sem database inicial) e `user` são reaproveitados.

```toml
[[connection]]
id = "…"
name = "snow-prod"
database = "ANALYTICS"
user = "DOUGLAS@EMPRESA.COM"

[connection.snowflake]
account = "xy12345.us-east-1"
auth = "externalbrowser"            # externalbrowser | keypair | pat
private_key = "~/.ssh/snowflake.p8" # só keypair
warehouse = "COMPUTE_WH"            # opcional
role = "ANALYST"                    # opcional
schema = "PUBLIC"                   # opcional
```

`SnowflakeConfig { account: String, auth: SnowflakeAuth, private_key: Option<String>, warehouse:
Option<String>, role: Option<String>, schema: Option<String> }`, `SnowflakeAuth { ExternalBrowser,
KeyPair, Pat }` (serde `lowercase`). Arquivos antigos continuam carregando.

**Segredos** (Keychain, serviço `dev.rustbeaver`): PAT na conta `<id>` (a mesma da senha do
Postgres); passphrase da `.p8` na conta `key:<id>`. SSO não guarda nada. Remover a conexão apaga
as duas contas.

**Diálogo de conexão**: seletor de tipo (Postgres | Snowflake). Snowflake mostra conta, usuário,
autenticação, chave privada (keypair), token (PAT, mascarado), passphrase da chave (keypair,
mascarada), warehouse, role, database e schema; esconde host, porta, SSL e SSH. Campos secretos em
branco mantêm o Keychain, como hoje.

## 3. Execução no `rb-core`

Biblioteca: `snowflake-connector-rs = "=1.1.0"` com features `key-pair-auth` e
`external-browser-sso`.

`Session` mantém a API pública (`execute`, `fetch_more(exec_id)`, `cancel`, `query_text`,
`run_in_transaction`); por dentro, `enum Backend { Postgres(..), Snowflake(..) }`. O código do
Postgres não muda de comportamento. Módulo novo `rb-core/src/snowflake.rs`.

**Sessão compartilhada.** Uma `SnowflakeSession` por conexão do app, guardada pelo `Engine` num
cache fraco (id da conexão + bloco Snowflake), no padrão do túnel SSH. Cada `Session` (árvore,
cada aba) tem seu próprio ator e cursor, todos sobre a mesma sessão HTTP. O SSO abre o navegador
uma vez por conexão. A sessão é criada com `CLIENT_SESSION_KEEP_ALIVE = true`; se mesmo assim
expirar, a próxima chamada refaz o login (no SSO, o navegador abre de novo).

**Execução e paginação.** `execute` envia o statement e devolve as primeiras `FETCH_ROWS` (1.000)
linhas; o cursor fica no ator. `fetch_more` entrega as próximas 1.000 do bloco atual e só baixa o
próximo bloco do conector quando o atual acaba. Um `fetch_more` com `exec_id` antigo devolve vazio
(como no Postgres). No Snowflake todo statement devolve linhas (DML devolve "number of rows
inserted", DDL devolve "status"), então o resultado é sempre mostrado na grade e `affected` fica vazio.

**Conversão.** Cada `CellValue` vira texto num `Batch` com colunas `Type::TEXT`:
- NULL → célula nula; booleano → `true`/`false`; inteiro e decimal → texto exato (decimal sem `f64`)
- float → formatação curta do Rust; `NaN`/`inf` como o Snowflake devolve
- date `YYYY-MM-DD`; time `HH:MM:SS[.fff]`; timestamp NTZ `YYYY-MM-DD HH:MM:SS[.fff]`;
  LTZ em UTC com `Z`; TZ com o deslocamento (`+03:00`)
- JSON (`VARIANT`/`OBJECT`/`ARRAY`) compacto; binário `\x` + hex minúsculo; vetor `[a, b, …]`

`ColumnInfo` ganha `type_name: Option<String>` (nome do tipo no Snowflake, ex. `NUMBER(38,0)`),
usado no cabeçalho da grade como dica.

**Cancelamento.** `cancel` usa o `QueryCanceller` da query em andamento daquela `Session`.

**Transações.** `BEGIN`/`START TRANSACTION` recusados com `DbError::Unsupported` (mesma regra do
Postgres): com sessão compartilhada, a transação afetaria todas as abas. `run_in_transaction`
devolve `DbError::Unsupported("Snowflake: somente leitura")`.

**Divisão de statements.** Lexer atual (`'…'`, `"…"`, `$$…$$`, `--`, `/* */`).

**Erros.** Variante nova `DbError::Snowflake { code: String, message: String }`, exibida como
`[code] message` (o conector não expõe o SQLSTATE). Falha de login e de rede vira `DbError::Connection`.

## 4. Metadados, árvore e autocomplete

Consultas (todas sem warehouse):
- databases: `SHOW DATABASES`
- schemas: `SHOW SCHEMAS IN DATABASE "DB"`
- objetos: `SHOW OBJECTS IN SCHEMA "DB"."S"` (coluna `kind`: TABLE, VIEW, MATERIALIZED_VIEW)
- funções: `SHOW USER FUNCTIONS IN SCHEMA "DB"."S"` (`arguments` formatado como `nome(tipos)`)
- colunas: `DESCRIBE TABLE "DB"."S"."T"` (nome, tipo, `null?`, `default`, `primary key`)
- contexto: `select current_database(), current_schema(), current_warehouse(), current_role()`
- seletor: `SHOW WAREHOUSES`, `SHOW ROLES`

`MetaCache` ganha `databases: Vec<String>`; no Snowflake, as chaves de schema são `DB.SCHEMA` e o
`search_path` é `[current_database.current_schema]`. Índices não existem no Snowflake (o grupo não
aparece). No Postgres nada muda.

**Árvore.** Raiz Snowflake = databases (ícone `database`) → schemas (`layers`) → grupos → objetos
→ colunas (com PK declarada em amarelo). Carregamento preguiçoso por nó, como hoje.

**Abrir tabela.** `select * from "DB"."SCHEMA"."T"` (+ filtro/ordem), identificadores sempre entre
aspas. A aba mostra o aviso fixo "Snowflake: somente leitura" e desativa + Linha, Excluir, Salvar.

**Autocomplete.** Tabelas e colunas do database/schema atuais e `SCHEMA.tabela` dentro do database
atual. `USE DATABASE`/`USE SCHEMA` no editor recarregam o contexto. Identificadores inseridos pela
regra do Snowflake: sem aspas se casam `^[A-Z_][A-Z0-9_$]*$` e não são reservados; senão entre
aspas (`quote_ident_snowflake`).

**Warehouse e role.** Dois seletores no topo da barra lateral quando a conexão ativa é Snowflake.
Escolher roda `USE WAREHOUSE "X"` / `USE ROLE "X"` na sessão compartilhada e recarrega o contexto.
Após cada `execute` com statement `USE …`, o app relê o contexto e atualiza os seletores.

## 5. Testes

**Unitários (sem rede):** conversão de cada `CellValue` (decimal de 38 dígitos, timestamps com e
sem fuso, JSON, binário, NULL); `quote_ident_snowflake`; parse de linhas de `SHOW`/`DESCRIBE`;
TOML com `[connection.snowflake]` ida e volta e arquivo antigo; recusa de `BEGIN`; paginação do
ator sobre blocos simulados (blocos de 700 e 2.300 linhas entregues em lotes de 1.000).

**Integração (conta real, opcional):** rodam só com `RB_TEST_SNOWFLAKE_ACCOUNT`,
`RB_TEST_SNOWFLAKE_USER`, `RB_TEST_SNOWFLAKE_WAREHOUSE`, `RB_TEST_SNOWFLAKE_ROLE`,
`RB_TEST_SNOWFLAKE_DATABASE` e um de `RB_TEST_SNOWFLAKE_PRIVATE_KEY` (caminho da `.p8`) ou
`RB_TEST_SNOWFLAKE_PAT`. Criam o schema `RB_TEST_<pid>_<n>` no database dado e o removem no fim.
1. `select 1`
2. 5.000 linhas de `table(generator(rowcount => 5000))` chegam em lotes de 1.000, em ordem
3. `cancel` interrompe `call system$wait(30)` em menos de 5 s
4. ida e volta de NUMBER(38,10), FLOAT, VARCHAR, BOOLEAN, DATE, TIME, TIMESTAMP_NTZ/LTZ/TZ,
   VARIANT, BINARY, NULL
5. metadados: schema de teste aparece; tabela, view e coluna com PK declarada
6. `USE ROLE` / `USE WAREHOUSE` mudam o contexto lido
7. erro de sintaxe vira `DbError::Snowflake` com código
8. duas `Session` da mesma conexão compartilham uma única sessão (um login)

**Manual** (`docs/manual-checklist.md`): login SSO no navegador (uma vez por conexão), diálogo
com tipo Snowflake, seletores de warehouse/role, árvore com ícones, grade somente leitura,
`⌘.` cancelando uma query longa.

## 6. Performance

- Conversão de 1.000 linhas × 10 colunas para `Batch`: < 5 ms (teste de bancada local)
- Navegar na árvore não liga warehouse (só `SHOW`/`DESCRIBE`)
- Memória de 1M linhas carregadas: mesma ordem do Postgres (o `Batch` é o mesmo)
