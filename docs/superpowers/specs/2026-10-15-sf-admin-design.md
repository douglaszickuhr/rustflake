# Snowflake: aba "Usuários & Roles" (somente leitura)

## Objetivo

Inspecionar usuários e roles de uma conexão Snowflake sem escrever SQL: propriedades do usuário
(role/warehouse padrão, desabilitado, último login, tipo de autenticação), roles diretas e
efetivas (herdadas, com caminho), quem tem uma role e o que ela concede, e atividade dos últimos
30 dias (logins, queries, gráfico diário).

## Fora do escopo

- Qualquer escrita: GRANT, REVOKE, ALTER USER, DISABLE. Fica para uma versão futura.
- Warehouses como objeto próprio.
- Postgres.
- Escolher outra role para a aba: usa a role atual da conexão.
- Lista das queries recentes do usuário.

## Decisões do usuário

| Pergunta | Escolha |
|---|---|
| Escopo | Somente leitura |
| Objetos | Usuários + roles |
| Entrada | Botão na barra lateral abre a aba com busca |
| Role usada | A role atual da conexão |
| Herança | Roles diretas + efetivas, com caminho |
| Atividade | Logins (30 d), queries (30 d), gráfico diário |

## Arquitetura

- **`crates/rb-core/src/admin.rs`** (novo, sem UI)
  - SQL builders e parsers da saída dos `SHOW`.
  - Busca de roles efetivas.
  - Funções `async` que recebem `&Session` e usam `query_named`.
- **`crates/rb-app/src/admin_tab.rs`** (novo)
  - `AdminTab`, que vira `TabView::Admin(Entity<AdminTab>)` em `workspace.rs`.
- **Sessão:** a aba abre a própria sessão, com o mesmo login e sem novo SSO, como `TableTab` e o indexer. Fica com a role atual da conexão.
- **Entrada:** botão "Usuários & Roles" (ícone de usuários) na barra lateral, só para conexão Snowflake aberta. Se a aba já existe para a conexão, o botão foca nela em vez de abrir outra.

## Dados (rb-core)

Identificadores entram sempre entre aspas duplas, com `"` dobrado (`quote_ident`).

### Lista de usuários

`SHOW USERS` → `Vec<UserInfo>`. A busca nas colunas é case-insensitive, e coluna ausente vira `None`.

| Campo | Coluna |
|---|---|
| `name` | `name` |
| `login_name` | `login_name` |
| `display_name` | `display_name` |
| `email` | `email` |
| `disabled` | `disabled` |
| `locked` | `snowflake_lock` |
| `default_role` | `default_role` |
| `default_warehouse` | `default_warehouse` |
| `default_namespace` | `default_namespace` |
| `created_on` | `created_on` |
| `last_success_login` | `last_success_login` |
| `has_password` | `has_password` |
| `has_rsa_public_key` | `has_rsa_public_key` |
| `has_mfa` | `has_mfa` |
| `user_type` | `type` |
| `comment` | `comment` |

Carregada uma vez ao abrir a aba. O filtro é local; o botão ↻ recarrega.

### Lista de roles

`SHOW ROLES` → `Vec<RoleInfo>` com `name`, `comment`, `owner`, `assigned_to_users` e `granted_to_roles`.

### Roles diretas do usuário

`SHOW GRANTS TO USER "X"` → `Vec<String>`, a coluna `role`.

### Grants de uma role

`SHOW GRANTS TO ROLE "R"` → `Vec<Grant>` com `privilege`, `granted_on`, `name` e `grant_option`.

As roles herdadas são as linhas com `privilege = USAGE` e `granted_on = ROLE`. O nome da role herdada vem em `name`.

### Quem tem uma role

`SHOW GRANTS OF ROLE "R"` → `Vec<(granted_to, grantee_name)>`, onde `granted_to` é `USER` ou `ROLE`.

### Roles efetivas

`effective_roles(direct, fetch)` faz uma busca em largura por nível:

- `fetch(role)` devolve as roles herdadas.
- O resultado é `Vec<EffectiveRole { name, path: Vec<String> }>`, onde `path` vai da role direta até a herdada.
- Cada role aparece uma única vez, com o primeiro caminho, que é o mais curto.
- Ciclos e diamantes não repetem nem travam.
- Roles diretas não entram na lista de efetivas (já aparecem acima).
- `fetch` é genérico (closure async), para teste com dados falsos. Na app, um cache por aba guarda `SHOW GRANTS TO ROLE` por role, e a tela de role reaproveita o mesmo cache.
- Os fetches de um mesmo nível rodam em paralelo (`join_all`).

### Atividade (30 dias)

Duas queries em `SNOWFLAKE.ACCOUNT_USAGE`, filtradas por `user_name = '<name>'`, com aspas simples escapadas. Os dados atrasam até cerca de 2 h.

**Logins (`LOGIN_HISTORY`)**

- Totais de sucesso e de falha.
- Último login com sucesso (`max(event_timestamp)`).
- Últimos 5 eventos: hora, sucesso, `first_authentication_factor`, `reported_client_type`, `client_ip` e `error_message`.

**Queries (`QUERY_HISTORY`)**

- Total de queries.
- Última query (`max(start_time)`).
- Soma de `total_elapsed_time`.
- Top 3 warehouses e top 3 roles por contagem.
- Contagem por dia (`date_trunc('day', start_time)`).

O resultado vira `Activity { logins, queries, per_day: Vec<(NaiveDate, u64)> }`. Os dias sem query são preenchidos com 0 no parser, para o gráfico ter 30 barras. Para manter o número de idas ao servidor baixo, as agregações podem ser `UNION ALL` ou queries separadas; isso é decisão do plano.

## UI (rb-app)

### Layout

- **Esquerda** (largura fixa):
  - Seletor Usuários | Roles e campo de filtro.
  - Lista virtualizada. Usuário desabilitado aparece em cinza com tag "desabilitado".
- **Direita:** os detalhes, com pilha de navegação e botão ←.

### Detalhes do usuário

**Cabeçalho:** nome, display name e e-mail.

**Propriedades** (`DescriptionList`):

- default role, warehouse e namespace
- desabilitado e bloqueado
- criado em
- último login (de `SHOW USERS`, disponível na hora)
- autenticação: senha, key pair, MFA, tipo

**Roles diretas:** tags clicáveis que abrem a role.

**Roles efetivas:** lista "READER via ANALYST → READER", também clicável.

**Atividade:**

- Carrega em segundo plano ao abrir o usuário, com spinner próprio e cache por usuário.
- Números de logins e queries, top warehouses e roles, e os 5 últimos logins.
- Gráfico de barras (`BarChart` do gpui-component) com 30 dias.
- Nota "dados com até ~2 h de atraso".

### Detalhes da role

- **Cabeçalho:** nome, comentário e owner.
- **Concedida a:** usuários e roles, clicáveis.
- **Herda:** roles com `USAGE` em `ROLE`, clicáveis.
- **Privilégios:** os demais grants no grid existente (`grid.rs`), somente leitura.

### Comportamento geral

- Todo texto de UI fica em pt/en/es (`crates/rb-app/locales/app.toml`). O `i18n_check` cobre as chaves.
- A aba segue a troca de idioma ao vivo, como as outras.

## Erros

- Cada seção (lista, propriedades, roles, efetivas, atividade, grants da role) mostra o próprio erro e não derruba as outras.
- Erro de privilégio ganha uma dica:
  - Lista e grants: "a role atual precisa de MANAGE GRANTS ou de ownership".
  - Atividade: "precisa de IMPORTED PRIVILEGES no database SNOWFLAKE".
  - Para reconhecer, procura `Insufficient privileges` / `does not exist or not authorized` na mensagem.
- Atividade sem warehouse ativo mostra o erro do Snowflake e a dica "defina um warehouse".
- Sessão expirada: tratada pela renovação que já existe na camada de sessão.

## Testes

Unitários em `rb-core/src/admin.rs`:

- `quote_ident`: aspas, maiúsculas e minúsculas, espaço.
- Escape de string em SQL.
- Parsers com fixtures no formato real das colunas: `SHOW USERS`, `SHOW ROLES`, `SHOW GRANTS TO USER`, `TO ROLE` e `OF ROLE`, incluindo coluna ausente e colunas em outra ordem.
- `effective_roles` com fetch falso:
  - cadeia
  - diamante (caminho mais curto, sem repetição)
  - ciclo (termina)
  - role direta herdada de novo (não aparece em efetivas)
- Atividade: preenchimento dos 30 dias e agregação a partir de linhas falsas.

Fora dos testes automáticos:

- Sem teste ao vivo contra Snowflake. A validação é manual na conta do usuário.
- A UI é verificada manualmente: abrir a aba, filtrar, navegar entre usuário e role, voltar, ver a atividade e o erro sem privilégio.

## Riscos

- `SHOW USERS` limitado a 10.000 linhas: suficiente; se cortar, mostra aviso.
- `LOGIN_HISTORY.USER_NAME` vs `login_name`: o plano verifica qual nome a view usa e filtra por esse.
- Cada usuário aberto dispara 2 queries em warehouse. O resultado fica em cache na aba.
