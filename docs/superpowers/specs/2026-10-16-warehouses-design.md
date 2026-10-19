# Aba Warehouses (Snowflake, somente leitura)

Projeto D do canvas "Rustflake UX Concepts" (board 3 · Object page), na versão somente leitura.

## Objetivo

Ver tudo de um warehouse sem escrever SQL:

- configuração;
- quem pode usar;
- créditos dos últimos 14 dias;
- quem usa, se está subdimensionado (fila e spill) e as queries mais lentas, com o plano real.

## Fora do escopo

- Qualquer escrita: Suspend/Resume, ALTER, formulário que gera SQL. O usuário escolheu "só os valores".
- Top roles.
- Query insights.
- Postgres.

## Decisões do usuário

| Pergunta | Escolha |
|---|---|
| Formulário | Só os valores, sem edição |
| Quem pode usar | Grants diretos + quantos usuários chegam por herança |
| Extras | Top usuários, fila & spill, queries mais lentas |

## Entrada

- O ícone **Warehouses** do trilho, hoje desabilitado ("em breve"), passa a abrir a aba "Warehouses" da conexão atual.
- Só aparece habilitado para Snowflake.
- Uma aba por conexão: abrir de novo foca a existente, como `open_admin`.
- A aba usa sessões próprias (mesmo login), como a de Usuários & Roles.

## Layout

### Lista à esquerda (260 px)

- Filtro e contador `N de M`.
- Por warehouse: o nome e a linha `XS · running · 3 queries` ou `XS · suspended`.
- ↻ recarrega.
- Ordem alfabética.

### Detalhes

**1. Cabeçalho**

- Nome.
- Pílula de estado: verde STARTED/RUNNING, cinza SUSPENDED, laranja RESIZING/SUSPENDING.
- `running N · queued M`.

**2. Configuração** (linhas chave/valor)

- tamanho;
- tipo (STANDARD / SNOWPARK-OPTIMIZED);
- auto-suspend (`60 s`, `10 min`, `nunca` quando 0 ou nulo);
- auto-resume;
- clusters `min–max · política` (single-cluster quando 1–1);
- owner, comentário;
- criado e retomado (`resumed_on`);
- resource monitor, quando houver.

**3. Quem pode usar**

- Roles com USAGE / OPERATE / MONITOR / MODIFY / OWNERSHIP, agrupadas por privilégio. Cada role é clicável e abre a role na aba Usuários & Roles (abre ou foca essa aba).
- "**N usuários** chegam a ele": usuários que têm, direta ou por herança, uma role com USAGE ou OWNERSHIP.
  - Expansível em lista; cada usuário abre a página dele em Usuários & Roles.

**4. Créditos · 14 dias**

- Faixa de 14 quadrados por intensidade, com tooltip `dia · X créditos`.
- Total do período.

**5. Uso · 14 dias** (carrega ao abrir o warehouse)

- **Top usuários**: 5, com nº de queries e tempo total; clicável.
- **Fila & spill**:
  - queries que esperaram na fila (n e % do total);
  - espera média e máxima;
  - spill local e remoto.
  - Aviso "pode estar subdimensionado" quando > 5% das queries esperaram ou houve spill remoto.
- **Queries mais lentas**: 10.
  - Cada uma com o começo do SQL (1 linha), usuário, tempo, data e o `Query ID` copiável.
  - Botão **Plano**: abre num diálogo o plano real, com o mesmo `PlanView` e perfil da aba SQL, via `Session::actual_plan`.
  - Sem privilégio (MONITOR no warehouse ou admin), o Snowflake devolve erro; o diálogo mostra o erro.

## Dados

| Seção | Fonte | Atraso |
|---|---|---|
| Lista, cabeçalho, configuração | `SHOW WAREHOUSES` | nenhum |
| Grants diretos | `SHOW GRANTS ON WAREHOUSE "X"` | nenhum |
| Usuários que chegam | grafo de herança (`GRANTS_TO_ROLES`, já usado) + `ACCOUNT_USAGE.GRANTS_TO_USERS` (`grantee_name`, `role`, `deleted_on is null`), uma vez por aba | ~2 h |
| Créditos | `TABLE(INFORMATION_SCHEMA.WAREHOUSE_METERING_HISTORY(...))`, com os 14 dias gerados no servidor (generator), como na atividade do usuário | quase nenhum |
| Uso | `ACCOUNT_USAGE.QUERY_HISTORY` com `warehouse_name = 'X'` e 14 dias, numa query com `UNION ALL` (linhas `kind, ...`, padrão das queries de atividade) | ~2 h |
| Plano | `GET_QUERY_OPERATOR_STATS` (+ `QUERY_HISTORY` do information_schema), já existe | nenhum |

- Identificadores com `quote_ident`; literais com `sql_str` (que escapa `\`).
- Sessões:
  - os SHOW numa sessão;
  - ACCOUNT_USAGE e créditos noutra, porque o ator é serial;
  - o plano do diálogo na sessão de uso.

## Erros

- Cada seção mostra o próprio erro e não derruba as outras.
- Dicas de privilégio:
  - grants: MANAGE GRANTS;
  - uso e usuários: IMPORTED PRIVILEGES;
  - créditos e uso sem warehouse ativo: "defina um warehouse";
  - plano: MONITOR no warehouse.
- `↻` limpa os caches com contador de geração. Resultados de tasks anteriores são descartados, como na aba de admin.

## Testes (rb-core, unitários)

- Parser de `SHOW WAREHOUSES`: colunas por nome, ordem trocada, coluna ausente, `auto_suspend` nulo/0, clusters.
- Formatação:
  - auto-suspend (`60 s`, `10 min`, `2 h`, nunca);
  - clusters (`1–1` → single, `1–3 · STANDARD`).
- Grants por privilégio a partir de `SHOW GRANTS ON WAREHOUSE`.
- Usuários que chegam: busca reversa no grafo (role com USAGE ← roles que a herdam ← usuários), com ciclo e diamante.
- Agregação do uso: top usuários, fila & spill (média, máx, %), lentas; e a regra do aviso de subdimensionado.
- Créditos: 14 dias em ordem; total.
- SQL builders: quoting/escape do nome do warehouse.

A UI é verificada manualmente:

- abrir pelo trilho;
- filtrar, selecionar;
- clicar numa role e num usuário (abre Usuários & Roles);
- botão Plano de uma query lenta;
- ↻;
- warehouse sem privilégio.
