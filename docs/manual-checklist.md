# Checklist manual da UI

Rode antes de cada release, com o Postgres de teste (`docker start rb-pg`).

- [ ] Nova conexão salva no TOML sem senha; senha no Keychain
- [ ] Conectar lista schemas; expandir carrega objetos, colunas e índices; F5 / ⌘R recarrega
- [ ] ⌘↩ roda o statement sob o cursor; ⌘⇧↩ roda o script
- [ ] Resultado grande pagina ao rolar; contador mostra `+` enquanto há mais
- [ ] ⌘. cancela `select pg_sleep(30)` em < 1 s
- [ ] Erro com posição sublinha o trecho e mostra SQLSTATE
- [ ] Script salvo sozinho em ~1 s; ⌘W move para `scripts/closed/`
- [ ] ⌘C copia célula (valor completo), linha e coluna (TSV)
- [ ] Autocomplete: tabelas após FROM, colunas por alias, aspas em nomes mistos, nada dentro de strings
- [ ] Tabela: filtro, ordenação no servidor, editar, NULL, inserir, excluir, ⌘S
- [ ] Erro ao gravar não altera nada e mantém edições
- [ ] Tabela sem PK é somente leitura
- [ ] Derrubar o Postgres (`docker restart rb-pg`) e rodar de novo: reconecta
- [ ] Célula com 5 MB de texto rola sem travar (`select repeat('x', 5000000)`)

## Túnel SSH e edição de conexão

- [ ] "+" mostra a seção SSH; com host SSH vazio a conexão funciona como antes.
- [ ] Digitar um alias do ~/.ssh/config e sair do campo mostra usuário, porta e chave resolvidos como placeholder.
- [ ] Conexão com túnel para um host fora do known_hosts abre "Host SSH desconhecido" com o fingerprint; "Confiar" conecta e grava a linha no ~/.ssh/known_hosts.
- [ ] Chave com passphrase: sem passphrase o erro diz "exige passphrase"; com ela, conecta; reabrir o app conecta sem pedir de novo (Keychain).
- [ ] ✎ abre o diálogo preenchido; salvar com senha em branco mantém a senha (reconectar funciona).
- [ ] ✎ na conexão ativa com edição pendente numa aba de tabela: recusa com aviso e nada muda.
- [ ] ✎ na conexão ativa sem pendências: salva, reabre os scripts e reconecta.
- [ ] Apagar o host SSH de uma conexão e salvar: passphrase some do Keychain (Acesso às Chaves, conta ssh:<id>).
- [ ] ⌘. cancela uma query longa que passa pelo túnel.
- [ ] Bastion real do autor: árvore, autocomplete, grade e edição funcionam.
