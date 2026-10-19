# Configurações da empresa: convenções fora do app

## Objetivo

O rustbeaver fica genérico (publicável). Convenções de uma empresa — nomes de role, tag de
classificação, regras das masking policies — saem do código e vão para um **configuração**: um arquivo
TOML privado, escolhido por conexão. A configuração da empresa do autor fica fora do repo.

Sucesso:
- Com a configuração da empresa, as telas mostram o que mostram hoje. Desvios intencionais:
  - classificação vence warehouse também em "o que libera";
  - "gerenciado" passa a ser SCIM (`owner`/`granted_by` termina em `_PROVISIONER`), não `contains("OKTA")`;
  - segmento `C<n>` fora de `u8` não cai para o próximo segmento; sufixo de departamento sem
    diferenciar maiúsculas.
- Sem configuração, nenhuma tela afirma algo falso; blocos que dependem de convenção somem.
- Nenhum nome interno da empresa no código, nos testes ou nos locales.
- O rótulo `C<n>` é genérico (fica no código).

## Formato

Um arquivo por configuração em `~/Library/Application Support/rustbeaver/configurations/*.toml`.
Fixture dos testes: `crates/rb-core/tests/fixtures/acme.toml`; `examples/configurations/acme.toml` é cópia.

```toml
name = "ACME"

# Tipo de role: a primeira regra que casar vence. Regex sem diferenciar maiúsculas.
[[roles]]
kind = "classification"
match = '(^|_)C(?P<level>\d+)(_|$)'      # grupo `level` obrigatório
[[roles]]
kind = "warehouse"
match = '^(?P<warehouse>.+?_WH)_(?:(?P<op>OP)|MONITOR)'   # `warehouse` obrigatório; `op` casou = operar
[[roles]]
kind = "base"
match = '^(?P<department>.+)_DEPARTMENT_BASE_ROLE$|BASE_ROLE'   # `department` opcional
[[roles]]
kind = "functional"
match = '_FR$'
[[roles]]
kind = "access"
match = '_AR$'
write = "_RW"                             # substring no nome: escrita

[classification]
tag = "GOVERNANCE.TAGS.DATA_CLASSIFICATION"   # DB.SCHEMA.NAME, exatamente 3 partes
labels = ["Public", "Internal", "Confidential", "Restricted"]   # índice = nível - 1
values = { "C-1 Public" = 1, "C-2 Internal" = 2, "C-3 Confidential" = 3, "C-4 Restricted" = 4, "CONFLICT" = 4, "UNCLASSIFIED" = 4 }

[masking]
global = "CLASSIFICATION_C{level}_MASKING_AR"
group = { role = "SHARED_C{level}_MASKING_AR", databases = "*_SHARED" }
scoped_suffix = "_C{level}_MASKING_AR"     # `<DB>` ou `<DB>_<SCHEMA>` + sufixo
forced_by = ["AI_AGENT_FR"]
```

Todas as seções são opcionais. Sem `[classification]` não há níveis nem tag; sem `[masking]`
não há escopo de desmascaramento.

### Validação ao carregar

Arquivo é rejeitado (com mensagem) se:
- TOML inválido ou `name` vazio;
- regex inválida, ou `kind` desconhecido;
- regra `classification` sem grupo `level`, ou `warehouse` sem grupo `warehouse`;
- `classification.tag` sem exatamente 3 partes.

`level` que não cabe em `u8` = a regra não casa. `department` ausente = o nome inteiro da role.

## Núcleo (`rb-core/src/org_config.rs`)

```rust
pub struct OrgConfig { /* name, regras compiladas, classification, masking */ }

impl OrgConfig {
    pub fn none() -> OrgConfig;                          // sem regras
    pub fn parse(toml: &str) -> Result<Addon, String>;
    pub fn name(&self) -> &str;
    pub fn classify(&self, role: &str) -> RoleKind;  // primeira regra que casa; senão Other
    pub fn warehouse_of(&self, role: &str) -> Option<(String, bool /*operate*/)>;
    pub fn department_of(&self, role: &str) -> String;
    pub fn writes(&self, role: &str) -> bool;
    pub fn has_kind(&self, k: RoleKind) -> bool;     // compara só a variante (discriminant)
    pub fn level_label(&self, level: u8) -> Option<&str>;
    pub fn max_level(&self) -> Option<u8>;           // = labels.len(); texto do "forced masking"
    pub fn tag_level(&self, value: &str) -> Option<u8>;
    pub fn is_class_tag(&self, db: &str, schema: &str, name: &str) -> bool; // 3 partes, sem caixa
    pub fn scope(&self, role: &str, level: u8, databases: &[String]) -> Scope;
    pub fn columns(&self, counts: &[TagCount], level: u8, scope: &Scope) -> u64;
    pub fn group_glob(&self) -> Option<&str>;        // texto do escopo de grupo
    pub fn forces_masking(&self, roles: &[String]) -> Vec<String>; // as que a pessoa tem; vazio = nada
    pub fn tag_counts_sql(&self) -> Option<String>;  // partes da tag em maiúsculas via sql_str
}

/// Lê `*.toml` em ordem alfabética; nome repetido: o primeiro vence, o outro vira erro.
pub fn load_all(dir: &Path) -> (Vec<OrgConfig>, Vec<String>);
pub fn org_configs_dir() -> PathBuf;
```

- `regex` entra como dependência direta do `rb-core` (`RegexBuilder::case_insensitive(true)`).
- `RoleKind` continua no `admin.rs`; só o mapeamento nome → tipo sai do código.
- `Scope::Collab` vira `Scope::Group` (também em `admin_tab.rs` ~766 e ~1296).
- Templates `global`/`group.role` comparados em maiúsculas, como hoje; o filtro de databases do
  grupo usa `metadata::glob_match`. A busca do maior prefixo de database continua no código.
- `masking::tag_counts(&s, &configuração)`; sem tag na configuração, não consulta.
- Saem como funções livres: `admin::classify_role`, `admin::group_roles`, `user_view::filter_tree`
  (sem uso fora de testes), `unlocks::level_label`, `masking::{scope, columns, tag_level,
  MASKING_FORCED_BY, forces_masking, tag_counts_sql}`. `unlock_items` recebe `&OrgConfig` e checa
  classificação antes de warehouse.
- `table_info::{parse_class_tags, parse_all_tags}` recebem `&OrgConfig` e usam `is_class_tag` (as 3
  partes); sem configuração nenhuma tag é classificação.

### Genérico (sem configuração)

- "Provisionado por SCIM (`OWNER`)" quando `owner` (usuário) ou `granted_by` (grant) termina em
  `_PROVISIONER`. `okta_managed` → `scim_managed`; `DirectGrant::from_okta` → `from_scim`; o
  literal `"Okta"` em `admin_tab.rs` ~1143 vira texto do locale.
- Perfis AWS: o atual/do shell primeiro, depois todos em ordem alfabética (sem `take(8)` nem `bi-`).
- Locales (`crates/rb-app/locales/app.toml`, en/pt/es):
  - `admin.okta`, `admin.okta_note`, `admin.source_okta`, `admin.okta_path` → SCIM, com `%{owner}`;
  - `admin.scope_collab` → `%{glob}`;
  - `admin.forced_masking` → `%{level}` (= `max_level`);
  - `admin.policy_note` sem nome de database/schema;
  - `admin.filter` com exemplos neutros.

## App

- **Config:** `SnowflakeConfig.configuration: Option<String>` = o `name` da configuração,
  `#[serde(default, skip_serializing_if = "Option::is_none")]`.
- **Conexão:** `ConnTarget` ganha `org: Arc<OrgConfig>`. É montado só em `Workspace::connect()`
  (~241, dentro do `spawn_in`; também atende a reconexão ao abrir o app): a configuração é carregado
  nesse bloco. Nome não encontrado ou arquivo quebrado: notificação (como `ws.connect_failed`) no
  ramo `Ok`, e `OrgConfig::none()`.
- Quem usa: `admin_tab.rs` e `table_sections.rs` via `conn.org`; nada novo nos construtores.
- **Diálogo (aba General, só Snowflake):** select "Nenhum" + configurações instalados; lê a pasta ao
  abrir o diálogo. Erros de arquivo aparecem na `description` do campo.
- Mudar a configuração de uma conexão aberta reconecta (é o que salvar já faz): SSO pode pedir login de
  novo e as abas da conexão fecham. Aceito.
- **Sem configuração, as telas:**
  - fatos do cabeçalho do usuário (warehouses, departamentos, dados) só com `has_kind`;
  - todas as roles em um grupo "Other";
  - Definition da tabela: sem "not classified" nem resumo de classificação;
  - desmascaramento nunca aparece (só existe para itens de classificação).

## Testes

- Testes atuais trocam nomes reais por ACME e usam a fixture via `include_str!`; a lógica testada
  é a mesma.
- Novos: regra sem grupo obrigatório rejeitada; regex inválida reportada; tag sem 3 partes; nomes
  repetidos; `OrgConfig::none()` → tudo Other e sem `tag_counts_sql`; aspas na tag escapadas;
  warehouse operate × monitor; `_WH_USAGE_AR` não é warehouse; classificação vence warehouse em
  `unlock_items`; `_PROVISIONER`.
- Equivalência: durante a implementação, um teste temporário compara regras antigas × configuração
  privado sobre uma lista de roles real (`$RB_ROLES_FILE`); sai junto com as funções antigas. Nada
  da empresa é commitado.

## Limpeza para publicar

- Fixtures, comentários e locales sem nomes internos (`dbt.rs`, `masking.rs`, `admin.rs`,
  `unlocks.rs`, `picker.rs`, `table_info.rs`, `user_view.rs`, `admin_tab.rs`).
- A configuração da empresa vai para `~/Library/Application Support/rustbeaver/configurations/` (fora do repo).

## Fora do escopo

- Repo novo com histórico limpo (o histórico atual tem nomes internos em 5+ commits).
- Specs e planos em `docs/superpowers/` que citam a empresa: listar, não reescrever.
- Hot reload de configurações; configurações com SQL próprio; tipos de role configuráveis.
