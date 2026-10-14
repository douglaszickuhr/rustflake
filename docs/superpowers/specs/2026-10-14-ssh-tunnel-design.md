# rustbeaver — túnel SSH e edição de conexão

Data: 2026-10-14
Status: aprovado em conversa, aguardando revisão da spec
Subprojeto 1 de 5 (1. túnel SSH + editar conexão, 2. várias conexões abertas,
3. editor: histórico/formatar/várias abas de resultado, 4. EXPLAIN visual, 5. outros bancos).

## 1. Objetivo

Conectar a bancos Postgres que só são alcançáveis por um bastion SSH, e alterar uma
conexão existente sem removê-la e recriá-la.

**Sucesso** = o autor conecta aos seus bancos reais através do bastion usando o
`~/.ssh/config` e as chaves que já tem (agente ou arquivo), sem digitar de novo o que o
config já diz, e edita uma conexão pelo botão ✎.

### Escopo

Dentro:
- Túnel SSH de um salto (máquina local → bastion → banco), dentro do processo, em Rust (`russh`)
- Autenticação por ssh-agent e por arquivo de chave (com ou sem passphrase)
- Leitura de `~/.ssh/config` (`ssh2-config`): `Host` com curingas, `Include`, `HostName`, `User`, `Port`, `IdentityFile`
- Verificação de `~/.ssh/known_hosts`, com confiança explícita em host novo
- Editar conexão (✎) no mesmo diálogo de criação

Fora:
- ProxyJump / ProxyCommand / múltiplos saltos
- Autenticação por senha SSH, keyboard-interactive, certificados SSH
- `Match`, `IdentityAgent`, `IdentitiesOnly` e demais diretivas não listadas acima (ignoradas)
- Encaminhamento por porta local (`-L`)

## 2. Modelo

`ConnectionConfig` ganha `ssh: Option<SshConfig>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`):

```toml
[[connection]]
id = "…"
name = "prod"
host = "db.internal"      # resolvido no bastion
port = 5432
database = "app"
user = "app"
ssl = "disable"

[connection.ssh]
host = "prod-bastion"     # alias do ~/.ssh/config ou hostname
user = "deploy"           # opcional
port = 22                 # opcional
identity_file = "~/.ssh/id_ed25519"   # opcional
```

`SshConfig { host: String, user: Option<String>, port: Option<u16>, identity_file: Option<String> }`.
Arquivos `connections.toml` sem o bloco continuam carregando.

### Resolução (`SshTarget`)

`SshConfig::resolve(ssh_config_path, env) -> SshTarget` produz os valores efetivos:

| Campo | 1º TOML | 2º `~/.ssh/config` (consulta pelo `host`) | 3º padrão |
|---|---|---|---|
| hostname | — | `HostName` | o próprio `host` |
| user | `user` | `User` | `$USER` |
| port | `port` | `Port` | 22 |
| chaves | `identity_file` | `IdentityFile` (todas) | `~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa` que existirem |

`SshTarget` também carrega `known_hosts: PathBuf` (padrão `~/.ssh/known_hosts`) e
`agent: Option<PathBuf>` (padrão `$SSH_AUTH_SOCK`). Ambos são parâmetros para que os
testes nunca toquem o `~/.ssh` real. `~` é expandido em todos os caminhos. Um
`~/.ssh/config` ausente ou ilegível equivale a vazio; erro de sintaxe vira `DbError::Ssh`.

### Segredos

Senha do banco: inalterada (Keychain, serviço `dev.rustbeaver`, conta = id).
Passphrase da chave: mesmo serviço, conta `ssh:<id>`. Remover a conexão apaga as duas.

## 3. Túnel

Módulo `rb-core/src/ssh.rs`.

- `Tunnel::connect(&SshTarget, passphrase: Option<String>) -> Result<Tunnel, DbError>`: TCP ao
  bastion (timeout 10 s), handshake, verificação do host, autenticação. Keepalive 30 s.
- `Tunnel::open_stream(&self, host, port) -> Result<ChannelStream, DbError>`: canal
  `direct-tcpip` convertido em stream (`channel.into_stream()`).
- `Tunnel::is_closed()`.

**Sem porta local.** O Postgres conecta sobre o stream com `Config::connect_raw(stream, tls)`;
TLS com o banco (quando `ssl` ≠ `disable`) roda por cima do canal com o mesmo conector
rustls de hoje (`MakeRustlsConnect::make_tls_connect(host)`).

**Integração com `Session`.** `Engine::connect` abre o `Tunnel` quando `cfg.ssh` existe e o
guarda na sessão (`Option<Arc<Tunnel>>`). Toda criação de cliente — conexão inicial,
reconexão automática quando o cliente fecha, cancelamento — abre um canal novo no túnel;
se `is_closed()`, reconecta o túnel antes. O cancelamento usa
`CancelToken::cancel_query_raw` sobre um canal próprio. O túnel cai junto com a `Session`
(desativar, editar, remover a conexão, sair do app).

### Autenticação

Ordem, imitando o OpenSSH:
1. Com chaves configuradas (TOML ou `IdentityFile`): para cada chave, se o agente tem a
   pública correspondente (`.pub` ao lado, ou derivada do privado sem passphrase), assina
   pelo agente; senão carrega o arquivo (com a passphrase, se cifrado).
2. Sem chaves configuradas: todas as identidades do agente, depois os arquivos padrão.

Limite conhecido: sem chave configurada, um agente com muitas chaves pode esgotar o
`MaxAuthTries` do servidor (igual ao OpenSSH). Solução do usuário: definir a chave.

### known_hosts

Consulta `[hostname]:port` (ou `hostname` na porta 22), inclusive entradas hasheadas (`|1|`).
- Conhecido e igual → segue.
- Desconhecido → `DbError::HostKeyUnknown { host, port, fingerprint }` (SHA256). O app
  pergunta "Confiar em <host> (SHA256:…)?"; ao confirmar, `ssh::trust_host` grava a linha
  no `known_hosts` e o app repete a conexão.
- Diferente → `DbError::Ssh` citando o arquivo e a linha em conflito. Sem opção de ignorar.

### Erros

Variantes novas em `DbError`: `Ssh(String)` e `HostKeyUnknown { host, port, fingerprint }`.
Mensagens:
- `bastion <host>:<port> inacessível: <causa>`
- `nenhuma chave aceita por <user>@<host> (tentadas: agente ×N, <arquivos>)`
- `passphrase incorreta para <arquivo>` / `<arquivo> exige passphrase`
- `o bastion recusou abrir canal para <db_host>:<db_port>` (AllowTcpForwarding ou destino)
- `chave do host <host> mudou (<known_hosts>:<linha>)`

## 4. Interface

**Diálogo de conexão** (`conn_dialog.rs`) ganha a seção "Túnel SSH": caixa "usar túnel",
campos host/alias, usuário, porta, chave e passphrase (mascarada). Usuário, porta e chave
mostram como placeholder o valor resolvido do `~/.ssh/config` para o host digitado
(recalculado ao sair do campo host).

**Editar.** Botão ✎ ao lado do × na barra lateral abre o mesmo diálogo preenchido,
mantendo o `id`. Senha e passphrase em branco mantêm o valor atual no Keychain; desligar
o túnel apaga a passphrase guardada. Se a conexão editada é a ativa, o app reconecta com
as regras de troca de conexão atuais (recusa com edições pendentes; salva os scripts).

**Confiança no host.** `HostKeyUnknown` vindo de qualquer conexão abre o diálogo de
confirmação; "Confiar" grava e repete a ação que falhou.

## 5. Testes

**Ambiente.** `scripts/test-sshd.sh` gera chaves ed25519 de teste (com e sem passphrase) em
`target/test-ssh/`, sobe o container `rb-sshd` (Alpine + OpenSSH, `AllowTcpForwarding yes`,
porta 52222) numa rede Docker compartilhada com `rb-pg`, e imprime o valor de `RB_TEST_SSH`.
Nenhuma chave é commitada. Testes de túnel só rodam com `RB_TEST_SSH` definido.

**Unitários:**
- resolução a partir de `ssh_config` de fixture: alias → HostName/User/Port/IdentityFile,
  `Host *`, precedência do TOML, padrões, `~` expandido, config ausente
- TOML com `[connection.ssh]` ida e volta; arquivo antigo sem o bloco carrega

**Integração (`rb-sshd`), destino `rb-pg:5432` (nome que só resolve no bastion):**
1. `select 1` pelo túnel com chave em arquivo
2. chave com passphrase: certa conecta, errada → "passphrase incorreta"
3. autenticação só pelo agente de teste (arquivo da chave inacessível)
4. host desconhecido → `HostKeyUnknown`; `trust_host` grava; nova tentativa conecta
5. chave do host trocada → recusa, `known_hosts` inalterado
6. destino `rb-pg:9` → canal recusado
7. `pg_terminate_backend` na sessão → próxima query reconecta pelo mesmo túnel
8. `cancel` interrompe `pg_sleep(30)` pelo túnel

**Manual** (`docs/manual-checklist.md`): seção SSH no diálogo, placeholders do config,
✎ com senha em branco mantendo a senha, diálogo de confiança, bastion real do autor.

## 6. Performance

O túnel não altera as metas da spec principal. A criptografia do canal custa CPU
proporcional aos bytes transferidos; o primeiro lote de `perf_big` pelo túnel local deve
ficar abaixo de 200 ms (medido, registrado em `docs/perf.md`).
