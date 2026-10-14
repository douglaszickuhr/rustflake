# rustbeaver — design

Data: 2026-10-14
Status: aprovado em conversa, aguardando revisão da spec

## 1. Objetivo

Cliente PostgreSQL desktop para macOS, escrito inteiramente em Rust, que substitui o
DBeaver Community no uso diário do autor (trabalho no `saas-backend`).

**Sucesso** = o autor usa o rustbeaver no lugar do DBeaver para escrever queries, navegar
no schema e editar dados, e o app cumpre as metas de performance da seção 8.

**Referência funcional:** DBeaver Community 26.2.2 (Apache-2.0, código em
`github.com/dbeaver/dbeaver`). Comportamento é estudado pelo código-fonte, não por
engenharia reversa do binário.

### Escopo

Dentro:
- PostgreSQL apenas (com TLS opcional)
- Editor SQL com destaque de sintaxe e autocomplete
- Navegador de schema (árvore)
- Grade de resultados virtualizada com paginação
- Edição de dados em tabelas com chave primária
- macOS apenas

Fora (adicionar só quando fizer falta):
- Outros bancos, diagramas ER, import/export, túnel SSH, IA, plugins, temas
- Modo de transação manual (só autocommit)
- Ordenação/edição em resultados de SQL livre
- Importar conexões do DBeaver
- Despejo de lotes antigos da memória
- Ranking fuzzy no autocomplete
- Testes automatizados de UI

## 2. Arquitetura

Workspace Cargo com dois crates:

```
rustbeaver/
  Cargo.toml            (workspace)
  crates/
    rb-core/            Rust puro, sem UI
    rb-app/             binário GPUI
```

- **rb-core**: conexões, execução de queries, paginação, metadados, geração de SQL de
  edição, config, Keychain. Não depende de GPUI. Testável sem janela.
- **rb-app**: janela, abas, editor, árvore, grade. Depende de `rb-core`.

Motivos da separação: testes de integração do core sem GPU, e isolar a API pré-1.0 do
GPUI em um só crate.

### Dependências principais (versões fixadas)

| Crate | Versão | Uso |
|---|---|---|
| `gpui-kit` | =0.7.3 (gpui-pre 0.3.9 + gpui-component 0.7.3 + plataforma macOS), feature `tree-sitter-sql` | UI, editor de código, tabela virtualizada, árvore |
| `tokio-postgres` | 0.7.x | protocolo Postgres |
| `tokio-postgres-rustls` | 0.14.x | TLS |
| `tokio` | 1.53.x | runtime de I/O |
| `security-framework` | 3.x | Keychain do macOS |
| `tracing` | 0.1.x | timings e logs |

### Modelo de threads

- A thread de UI do GPUI **nunca** faz I/O.
- `rb-core` sobe um runtime tokio em uma thread dedicada.
- A UI envia comandos e recebe resultados em lotes por canais. A grade renderiza o
  primeiro lote antes da query terminar.

### Configuração

- Conexões: `~/Library/Application Support/rustbeaver/connections.toml`
  (host, porta, banco, usuário, sslmode). Sem senha no arquivo.
- Senhas: Keychain do macOS, chave = id da conexão.
- Scripts SQL: `~/Library/Application Support/rustbeaver/scripts/*.sql`.

## 3. Camada de banco (rb-core)

### Conexões

- 1 conexão dedicada a metadados.
- 1 conexão por aba (editor SQL ou tabela aberta), aberta sob demanda e fechada com a aba.
- Query longa em uma aba não bloqueia árvore nem outras abas.

### Divisão de statements

O script é dividido em statements por um lexer SQL próprio e pequeno (também usado pelo
autocomplete), nunca por `;` textual. Deve respeitar blocos `$$...$$`, `$tag$...$tag$`,
strings, `E'...'`, identificadores entre aspas e comentários (inclusive aninhados).

- `Cmd+Enter`: executa o statement sob o cursor.
- `Cmd+Shift+Enter` (executar script): roda os statements em sequência, parando no primeiro erro.

### Execução

Todo statement é preparado; a classificação vem do próprio Postgres:

- **Retorna colunas** (`SELECT`, `WITH`, `VALUES`, `... RETURNING`, `EXPLAIN` etc.):
  dentro de `BEGIN`, faz bind de um portal e busca lotes de 1.000 linhas
  (`query_portal(portal, 1000)`). O primeiro lote vai imediatamente para a UI; os
  seguintes são pedidos quando a grade rola perto do fim. A transação recebe `COMMIT`
  quando o portal esgota, quando chega outro comando na sessão, ou quando a aba fecha.
  `COMMIT` (e não `ROLLBACK` em `READ ONLY`) preserva a semântica de autocommit:
  `nextval()`, funções que escrevem e `SELECT ... FOR UPDATE` continuam funcionando.
- **Não retorna colunas** (DML sem `RETURNING`, DDL, `VACUUM`): executa em autocommit e
  devolve a contagem de linhas afetadas.

### Armazenamento de resultados

- Cada lote = um buffer contíguo de bytes crus (formato binário do Postgres) + tabela
  de offsets por célula (`None` = NULL).
- Captura via um tipo que implementa `FromSql` aceitando qualquer tipo e guardando os
  bytes.
- Decodificação para texto só das células visíveis, no momento do desenho.
- Tipos suportados na decodificação: inteiros, numeric, float, bool, text/varchar/char,
  uuid, date/time/timestamp/timestamptz/interval, json/jsonb, bytea (hex), arrays desses,
  enums (como texto). Qualquer outro: `<nome_do_tipo: N bytes>`.
- Lotes nunca são liberados enquanto o resultado estiver aberto.
- TLS: `prefer` e `require` verificam o certificado com as raízes do sistema (mais
  estrito que o `require` do libpq, que não verifica).

### Cancelamento

`Cmd+.` envia cancel request via `Client::cancel_token()` para a query da aba. Sem
timeout automático.

### Metadados

- Consultas direto em `pg_catalog` (não `information_schema`).
- Objetos: schemas, tabelas, views, views materializadas, colunas (nome, tipo, nullable,
  default), chave primária, índices, funções.
- Carga preguiçosa por nó da árvore; cache em memória compartilhado com o autocomplete.
- `F5` recarrega o nó selecionado e invalida o cache dele.

### Transações

Somente autocommit (exceto o portal de leitura e a gravação da grade, que usam transações
internas curtas).

## 4. Interface (rb-app)

### Layout

```
┌──────────────┬─────────────────────────────────────────┐
│ conexão ▾    │ [script-1.sql] [public.users] [+]       │
│ ▸ public     ├─────────────────────────────────────────┤
│   ▸ Tables   │  editor SQL                             │
│     users    │                                         │
│   ▸ Views    ├─────────────────────────────────────────┤
│   ▸ Functions│  grade de resultados (virtualizada)     │
│              │ 1.000+ linhas · 42 ms · ⏹ Cmd+.         │
└──────────────┴─────────────────────────────────────────┘
```

- Barra lateral: seletor de conexão + árvore de schema.
- Área principal: abas de SQL e de tabelas.
- Diálogo simples para criar conexão (salva no TOML + Keychain). Editar ou remover: pelo
  TOML por enquanto.

### Editor SQL

- Editor de código do `gpui-component` com destaque de SQL.
- Cada aba é um arquivo `.sql`, salvo automaticamente (debounce de ~1 s e ao fechar).
  Ao conectar, os scripts de `scripts/` reabrem como abas. Fechar a aba (`Cmd+W`) move o
  arquivo para `scripts/closed/` — nada é apagado.
- Autocomplete, só a partir do cache (sem I/O ao digitar):
  - após `FROM` / `JOIN` / `UPDATE` / `INTO`: tabelas e views;
  - após `identificador.`: colunas da tabela, resolvendo alias pelo `FROM` do statement
    atual; se o identificador for um schema, os objetos dele;
  - demais posições: palavras-chave + tabelas.
  - Metadados fora do cache disparam carga em segundo plano; os itens aparecem na próxima
    tecla.
  - Filtro: prefixo primeiro, depois "contém", sem diferenciar maiúsculas.

### Grade

- `Table` virtualizada do `gpui-component`.
- Pede próximo lote quando faltam < 200 linhas para o fim.
- Largura das colunas estimada pelo primeiro lote (com limite máximo).
- `Cmd+C` copia a seleção: célula (valor completo), linha ou coluna (TSV). O componente
  não tem seleção de intervalo de células.
- Rodapé: contagem de linhas carregadas (com `+` se houver mais), tempo de execução,
  botão de cancelar.

### Tabela aberta pela árvore

- Executa `SELECT * FROM "schema"."tabela" [WHERE <filtro>] [ORDER BY <col> ASC|DESC]`
  com a mesma paginação.
- Campo de filtro aceita expressão `WHERE` crua.
- Clique no cabeçalho alterna ordenação no servidor.

### Edição de dados

- Somente em tabelas abertas pela árvore **com chave primária**. Sem PK: somente leitura
  com aviso.
- Duplo clique na célula abre um diálogo com o valor completo (editor multilinha), com
  "OK", "Definir NULL" e "Cancelar".
- Edições ficam pendentes e destacadas: célula alterada, linha nova, linha marcada para
  exclusão. Recarregar, filtrar ou ordenar com edições pendentes é recusado.
- `Cmd+S` grava tudo em uma transação (exclusões, alterações, inserções, nessa ordem); cada
  statement precisa afetar exatamente 1 linha, senão tudo é desfeito:
  - `UPDATE "s"."t" SET "c" = ($1::text)::<tipo> WHERE pk = ...`
  - `INSERT` e `DELETE` equivalentes, parametrizados.
  - Valores sempre enviados como texto e convertidos pelo Postgres
    (`($n::text)::<tipo>`), cobrindo enum, json, array, uuid etc.
- Erro: `ROLLBACK`, edições continuam pendentes, erro do Postgres exibido.
- Botão "descartar" limpa as edições pendentes.
- Sem detecção de conflito concorrente.

### Atalhos

| Atalho | Ação |
|---|---|
| `Cmd+Enter` | executar statement sob o cursor |
| `Cmd+Shift+Enter` | executar script inteiro |
| `Cmd+.` | cancelar query |
| `Cmd+S` | gravar edições da grade |
| `Cmd+T` | nova aba SQL |
| `Cmd+W` | fechar aba |
| `F5` | recarregar nó da árvore |

## 5. Tratamento de erros

- `rb-core` não entra em pânico; todas as operações retornam um enum de erro próprio
  (conexão, query, Keychain, config, I/O).
- Erro do Postgres exibe SQLSTATE, mensagem, `DETAIL`, `HINT`. Se houver posição, o
  editor sublinha o trecho.
- Conexão perdida: aba marcada como desconectada; reconecta automaticamente na próxima
  execução. A query interrompida **não** é reenviada.
- Falha ao ler config ou Keychain: mensagem na UI, app continua abrindo.

## 6. Testes

- **Unitários (rb-core, funções puras):** divisão de statements (`$$`, `$tag$`, strings,
  comentários), contexto do autocomplete e resolução de alias, geração de SQL de edição,
  decodificação de cada tipo suportado.
- **Integração (rb-core):** Postgres em Docker, URL em `RB_TEST_DATABASE_URL`; testes são
  ignorados se a variável não existir. Cobrem: paginação de 1M linhas, cancelamento,
  gravação com PK, rollback em erro, tipo desconhecido, reconexão.
- **rb-app:** checklist manual em `docs/manual-checklist.md`.

## 7. Observabilidade

`tracing` registra: início do processo → janela visível, conexão estabelecida, tempo até
primeiro lote, tempo total de query, tempo de abertura do autocomplete. Nível controlado
por `RUST_LOG`.

## 8. Metas de performance

Medidas no Mac do autor, Postgres local. Tempo via `tracing`, memória via
`/usr/bin/time -l`.

| Métrica | Meta |
|---|---|
| Abrir o app até janela visível | < 300 ms |
| Memória parada, 1 conexão | < 150 MB |
| Primeiras linhas de `SELECT` sobre 1M linhas | < 200 ms |
| Rolagem da grade com 1M linhas | nenhum frame > 16 ms |
| Popup de autocomplete, 1.000 tabelas no cache | < 16 ms |
| Memória com 1M × 10 colunas carregadas | < 400 MB |

## 9. Riscos

- **API do GPUI pré-1.0:** versões fixadas; mudanças ficam contidas em `rb-app`.
- **Componentes do `gpui-component` podem não cobrir algo** (ex.: seleção de células
  na tabela, posição de erro no editor). Mitigação: verificar cada componente no
  primeiro passo do plano antes de construir em cima dele.
- **Gramática tree-sitter de SQL não cobrir sintaxe Postgres** (ex.: dollar quoting).
  Mitigação: testes da divisão de statements; se falhar, divisor manual pequeno só para
  delimitação.
