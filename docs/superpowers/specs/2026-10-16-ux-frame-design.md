# UX A: Moldura do app (canvas "Rustflake UX Concepts", board 1)

Fonte visual: https://claude.ai/artifact/KeTaipPVZnaCRzi68f6V5p, board `Main.dc.html`.

## Objetivo

Trocar a barra lateral atual pela moldura do canvas:

- barra de título com abas, contexto e ⌘K;
- trilho de ícones;
- painel de objetos;
- barra de status;
- ambiente por conexão;
- paleta e fonte do canvas.

O editor e os resultados não mudam (ficam para o B).

## Fora do escopo

- Resultado inline por statement, comparar, Record/JSON (B).
- Access explainer (C).
- Página de warehouse (D).
- Ícone "Scripts" no trilho: as abas já são os scripts.
- Paleta de comandos com escrita (board 2): descartada.

## Decisões do usuário

| Pergunta | Escolha |
|---|---|
| Como saber que é produção | Campo na conexão |
| Tema | Escuro + claro, seguindo o macOS (o seletor manual atual continua) |
| Onde ficam as conexões | No menu da barra de contexto |
| Ordem dos projetos | A → B → C, D só leitura |

## 1. Ambiente por conexão

**Config (rb-core `config.rs`)**

- `ConnectionConfig.environment: Option<Environment>`, com `enum Environment { Production, Staging, Development }`.
- Serde: `rename_all = "lowercase"`, `default`, `skip_serializing_if = "Option::is_none"`.
- Arquivo antigo sem o campo carrega como `None`.

**Diálogo de conexão:** seletor "Ambiente": nenhum / Produção / Staging / Desenvolvimento.

**Rótulo e cor:**

| Ambiente | Rótulo | Cor |
|---|---|---|
| Production | `PROD` | laranja `#F0A35E` |
| Staging | `STAGING` | neutra |
| Development | `DEV` | neutra |
| None | sem rótulo | sem borda |

**Borda de 2px no topo da janela:**

- Laranja quando a aba ativa é de uma conexão Production.
- Nos outros casos, a cor de borda do tema.

## 2. Paleta e fonte

O tema claro/escuro/auto atual (`appearance.rs`) continua. Depois do `Theme::change`, as cores do gpui-kit são sobrescritas pelas do canvas.

**Escuro**

| Token | Cor |
|---|---|
| background | `#0B0B0C` |
| superfícies (input, popover, abas) | `#111113` |
| secondary / hover | `#17171A` |
| border | `#1D1D21` |
| border forte (inputs) | `#26262B` |
| foreground | `#E8E8EC` |
| muted_foreground | `#9A9AA3` |
| texto terciário | `#7C7C85` |
| accent / primary | `#6CC4F0` |
| prod | `#F0A35E` |

**Claro** (derivado, mesma estrutura)

| Token | Cor |
|---|---|
| background | `#FAFAFA` |
| superfícies | `#FFFFFF` |
| secondary | `#F0F0F2` |
| border | `#E4E4E8` |
| border forte | `#D4D4DA` |
| foreground | `#141416` |
| muted_foreground | `#5E5E66` |
| accent / primary | `#1F7FB0` (o azul do escuro escurecido para contraste 4,5:1 em texto) |
| prod | `#C46A1E` |

A paleta mora numa função pura, `palette(mode) -> Palette`, testada: cada par de texto/fundo tem contraste de pelo menos 4,5:1.

**Fontes**

- Geist e Geist Mono (OFL) entram no binário (`assets/fonts`) e são registradas com `text_system().add_fonts` na partida.
- `theme.font_family = "Geist"`; `mono_font_family = "Geist Mono"` (editor e grade).
- Se o registro falhar, fica a fonte do sistema e sai um aviso no log.

## 3. Layout

**Janela:** barra de título transparente (`TitlebarOptions { appears_transparent: true, traffic_light_position }`). O conteúdo começa a 84px da esquerda na linha do título.

**Linha do título** (44px)

- Esquerda: as abas, o mesmo `TabBar` de hoje, mais compacto, com + no fim.
- Centro: a barra de contexto.
- Direita: o botão "Ir para qualquer coisa ⌘K", que dispara o `QuickOpen` atual.
- Área vazia da linha arrasta a janela.

**Barra de contexto**

- Mostra a conexão da aba ativa, ou da conexão atual se não houver aba.
  - Snowflake: `ENV / ROLE / WAREHOUSE / DB.SCHEMA`.
  - Postgres: `ENV / user / database.schema`.
- Partes ausentes viram `—`.
- Sem conexão: "Conectar…".
- O clique abre um menu (popover) com seções:
  - **Conexões:** lista com a atual marcada; clicar conecta ou troca a atual. Por item: editar, desconectar, remover. No fim: "+ Nova conexão".
  - **Role** e **Warehouse** (só Snowflake): os mesmos `pick_context` de hoje.
  - **Database / Schema:** o `USE` correspondente, pelo mesmo caminho dos chips atuais.

**Trilho** (52px, à esquerda, abaixo do título)

| Botão | Ação |
|---|---|
| Objetos | Abre e fecha o painel de objetos (aberto por padrão; o estado vale para a sessão) |
| Acesso | Abre a aba Usuários & Roles da conexão atual. Só aparece para Snowflake. |
| Warehouses | Desabilitado, com tooltip "em breve" (D) |
| Histórico | Abre o ⌘Y |
| Idioma (pé) | O seletor atual |
| Tema (pé) | O seletor atual |

Cada botão tem `aria_label` e tooltip.

**Painel de objetos** (redimensionável, 180–640px, padrão 260px)

- Cabeçalho "DATABASES", com o funil do filtro (Snowflake).
- A árvore da conexão atual.
- Sem conexão: texto pedindo para conectar.

**Área principal**

- Igual à de hoje: aba ativa ou estado vazio.
- O estado vazio sem conexão mostra a lista de conexões em linhas, com Conectar, Editar, Remover e "+ Nova conexão". Isso substitui a barra lateral para quem acabou de abrir o app.

**Barra de status** (28px, embaixo, largura toda)

- Esquerda: o resumo da última execução da aba ativa, se ela expuser um, por exemplo "2 statements · 391 ms". Abas de tabela e de admin não mostram nada.
- Direita, dicas: `⌘↵ statement`, `⌘⇧↵ tudo`, `⌘E explain`, `⌘K`.

**A barra lateral atual some.** Cada coisa vai para:

| O que era | Para onde vai |
|---|---|
| Conexões | Menu da barra de contexto + estado vazio |
| Chips de warehouse e role | Barra de contexto |
| Filtro e árvore | Painel de objetos |
| Idioma e tema | Pé do trilho |
| Usuários & Roles | Trilho |

## 4. Erros

- Falha ao trocar role, warehouse ou db: a notificação de erro atual.
- Ambiente inválido no TOML: o campo vira `None` e sai um aviso no log. O arquivo inteiro continua carregando.
  - Se o serde não permitir isso fácil, falhar como hoje com a mensagem do TOML, que é aceitável.

## 5. Testes

**Unitários (rb-core / rb-app)**

- `environment`: ida e volta pelo TOML; arquivo sem o campo; valor inválido.
- Ambiente → rótulo e cor.
- Texto da barra de contexto: Snowflake completo, Postgres, partes ausentes, sem conexão.
- `palette`: contraste de pelo menos 4,5:1 nos pares de texto/fundo, nos dois modos.
- `i18n_check` com as chaves novas.

**Manual** (pedir ao usuário):

- conectar pelo estado vazio;
- trocar conexão, role e warehouse pelo menu;
- borda e rótulo PROD;
- trilho;
- painel abre e fecha;
- tema claro/escuro;
- a fonte Geist.
