# Medindo as metas de performance

Preparação: `docker exec -i rb-pg psql -U postgres < scripts/seed-perf.sql`.
Sempre com build de release: `cargo build --release -p rb-app`.

| Métrica | Meta | Como medir |
|---|---|---|
| Abrir até janela visível | < 300 ms | `RUST_LOG=info target/release/rustbeaver` → log `janela aberta elapsed_ms` |
| Memória parada, 1 conexão | < 150 MB | conectar, esperar 10 s, `ps -o rss= -p $(pgrep rustbeaver)` (KB) |
| Primeiro lote de `select * from perf_big` | < 200 ms | log `primeiro lote elapsed_ms` |
| Rolagem com 1M linhas | nenhum frame > 16 ms | carregar tudo (rolar até o fim), rolar rápido com Instruments → template "Animation Hitches" |
| Autocomplete com 1.000 tabelas | < 16 ms | `RUST_LOG=rustbeaver=debug,info` → log `autocomplete us` digitando `select * from perf_t` |
| Memória com 1M × 10 colunas carregadas | < 400 MB | núcleo: `cargo test --release -p rb-core --test perf -- --ignored --nocapture`; somar a memória parada da UI |

Registre os resultados na tabela abaixo a cada medição.

| Data | Commit | Janela | Parado | 1º lote | Rolagem | Autocomplete | 1M×10 |
|---|---|---|---|---|---|---|---|
| 2026-10-14 | d268912 | 121–219 ms (1ª execução fria: 1021 ms) | 96 MB | 4 ms | não medido | 143 µs | 253 MB núcleo (~350 MB com UI) |

Notas da medição de 2026-10-14:
- A primeira execução depois de compilar levou 1 s (cache de fontes/shaders frio); as seguintes, 121–219 ms.
- Rolagem não medida: precisa de Instruments com interação humana (a automação de UI usada não entrega rolagem ao GPUI).
- 1M × 10 medido no núcleo (ResultSet completo); a soma com a UI é estimativa.
