//! Plano de execução: modelo comum, parsers (Postgres e Snowflake) e layout do diagrama.

use rust_i18n::t;
use serde_json::Value;

use crate::DbError;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanNode {
    /// "Hash Join", "TableScan".
    pub op: String,
    /// Tabela/índice/função.
    pub object: Option<String>,
    /// Filtro, condição de join, buffers, partições, spill…
    pub detail: Vec<(String, String)>,
    pub est_rows: Option<f64>,
    pub act_rows: Option<f64>,
    /// Exclusivo (estimado). No Snowflake estimado guarda os bytes lidos.
    pub cost: Option<f64>,
    /// Exclusivo (real), já multiplicado pelos loops.
    pub time_ms: Option<f64>,
    /// 0..=100: peso do nó para cor e destaque.
    pub pct: f64,
    pub warn: Vec<String>,
    pub children: Vec<PlanNode>,
    /// `[id]` do Snowflake (o mesmo do Query Profile).
    pub id: Option<i64>,
    /// Segunda linha da caixa: condição do join/filtro, tabela, chaves.
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub root: PlanNode,
    pub analyzed: bool,
    pub total_ms: Option<f64>,
    /// Por que os números estão vazios, quando estão.
    pub note: Option<PlanNote>,
    /// Só no real do Snowflake: o que o Query Profile mostra ao lado do diagrama.
    pub profile: Option<Profile>,
}

/// Agregados de GET_QUERY_OPERATOR_STATS (todos os operadores) + tempos do QUERY_HISTORY.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Profile {
    /// Categoria → fração do tempo total (soma dos operadores), maior primeiro.
    pub breakdown: Vec<(String, f64)>,
    pub bytes_scanned: f64,
    /// Fração lida do cache do warehouse, ponderada pelos bytes.
    pub from_cache: Option<f64>,
    /// O scan mais atrasado (1.0 = terminou).
    pub scan_progress: Option<f64>,
    pub partitions_scanned: f64,
    pub partitions_total: f64,
    pub spilled_local: f64,
    pub spilled_remote: f64,
    pub network_bytes: f64,
    pub timing: Option<Timing>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timing {
    pub compile_ms: f64,
    pub queued_ms: f64,
    pub exec_ms: f64,
    pub total_ms: f64,
}

/// Tempos da query pelo id (INFORMATION_SCHEMA: sem o atraso do ACCOUNT_USAGE).
pub fn query_timing_sql(query_id: &str) -> String {
    format!(
        "select compilation_time, queued_provisioning_time + queued_repair_time + queued_overload_time as queued, execution_time, total_elapsed_time \
from table(information_schema.query_history(result_limit => 1000)) where query_id = '{}'",
        query_id.replace('\'', "")
    )
}

pub fn parse_timing(names: &[String], rows: &[Vec<Option<String>>]) -> Option<Timing> {
    let r = rows.first()?;
    let n = |c: &str| names.iter().position(|x| x.eq_ignore_ascii_case(c)).and_then(|i| r.get(i).cloned().flatten()).and_then(|v| v.parse::<f64>().ok());
    Some(Timing { compile_ms: n("COMPILATION_TIME")?, queued_ms: n("QUEUED").unwrap_or(0.0), exec_ms: n("EXECUTION_TIME")?, total_ms: n("TOTAL_ELAPSED_TIME")? })
}

/// Caminhos dos `n` nós mais caros (maior % primeiro).
pub fn top_nodes(root: &PlanNode, n: usize) -> Vec<Vec<usize>> {
    fn walk(node: &PlanNode, path: &mut Vec<usize>, out: &mut Vec<(f64, Vec<usize>)>) {
        out.push((node.pct, path.clone()));
        for (i, c) in node.children.iter().enumerate() {
            path.push(i);
            walk(c, path, out);
            path.pop();
        }
    }
    let mut all = Vec::new();
    walk(root, &mut Vec::new(), &mut all);
    all.retain(|(p, _)| *p > 0.0);
    all.sort_by(|a, b| b.0.total_cmp(&a.0));
    all.into_iter().take(n).map(|(_, p)| p).collect()
}

/// Atributo mais informativo do operador, na ordem em que o Query Profile mostra.
fn summary_of(at: &Value) -> Option<String> {
    ["table_name", "equality_join_condition", "filter_condition", "additional_join_condition", "grouping_keys", "sort_keys", "functions", "input_expressions"]
        .iter()
        .find_map(|k| at.get(*k).map(text).filter(|s| !s.is_empty()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanNote {
    /// Nenhum nó com custo: o estimado do Snowflake só tem bytes de TableScan, e secure views escondem o interior.
    NoCost { secure_views: bool },
    /// GET_QUERY_OPERATOR_STATS vazio: o resultado veio do cache, nada executou.
    Cached,
}

/// Todos os nós sem peso → explica (em vez de mostrar 0% em tudo).
fn no_cost_note(root: &PlanNode) -> Option<PlanNote> {
    fn all_zero(n: &PlanNode) -> bool {
        n.pct == 0.0 && n.children.iter().all(all_zero)
    }
    fn secure(n: &PlanNode) -> bool {
        n.op.contains("SecureView") || n.children.iter().any(secure)
    }
    all_zero(root).then(|| PlanNote::NoCost { secure_views: secure(root) })
}

/// Posição de um nó no diagrama; `parent` é o índice do pai neste mesmo vetor.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    pub path: Vec<usize>,
    pub x: f32,
    pub y: f32,
    pub parent: Option<usize>,
}

fn num(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(Value::as_f64)
}

fn bad_json(e: impl std::fmt::Display) -> DbError {
    DbError::Unsupported(t!("err.explain_parse", error = e).to_string())
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(text).collect::<Vec<_>>().join(", "),
        o => o.to_string(),
    }
}

const PG_DETAIL: &[&str] = &[
    "Filter", "Index Cond", "Recheck Cond", "Hash Cond", "Merge Cond", "Join Filter", "Rows Removed by Filter", "Sort Key", "Group Key", "Join Type", "Strategy",
    "Workers Planned", "Workers Launched", "Shared Hit Blocks", "Shared Read Blocks", "Temp Read Blocks", "Temp Written Blocks", "Peak Memory Usage",
];

fn children_of(v: &Value) -> &[Value] {
    v.get("Plans").and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// Tempo inclusivo de um nó: o do Postgres é por loop.
fn pg_incl_time(v: &Value) -> Option<f64> {
    num(v, "Actual Total Time").map(|t| t * num(v, "Actual Loops").unwrap_or(1.0))
}

fn pg_node(v: &Value, analyzed: bool) -> PlanNode {
    let kids = children_of(v);
    let loops = num(v, "Actual Loops").unwrap_or(1.0);
    let object = match (v.get("Relation Name").and_then(Value::as_str), v.get("Index Name").and_then(Value::as_str)) {
        (Some(r), Some(i)) => Some(format!("{r} ({i})")),
        (Some(r), None) => Some(r.to_string()),
        (None, Some(i)) => Some(i.to_string()),
        (None, None) => v.get("Function Name").or(v.get("CTE Name")).and_then(Value::as_str).map(str::to_string),
    };
    let detail = PG_DETAIL.iter().filter_map(|k| v.get(*k).filter(|x| !x.is_null()).map(|x| (k.to_string(), text(x)))).collect();
    let est = num(v, "Plan Rows");
    let act = if analyzed { num(v, "Actual Rows").map(|r| r * loops) } else { None };
    let mut warn = Vec::new();
    if let (Some(e), Some(a)) = (est, act)
        && e.max(a) / e.min(a).max(1.0) > 10.0
    {
        warn.push(t!("plan.warn_estimate", est = short_count(e), act = short_count(a)).to_string());
    }
    if v.get("Node Type").and_then(Value::as_str) == Some("Seq Scan")
        && let (Some(removed), Some(a)) = (num(v, "Rows Removed by Filter"), act.or(est))
        && removed > 9.0 * a.max(1.0)
    {
        warn.push(t!("plan.warn_seq_filter", n = short_count(removed)).to_string());
    }
    let child_time: f64 = kids.iter().filter_map(pg_incl_time).sum();
    let child_cost: f64 = kids.iter().filter_map(|c| num(c, "Total Cost")).sum();
    PlanNode {
        // INSERT/UPDATE/DELETE vêm como "ModifyTable"; o texto do Postgres mostra a operação.
        op: match v.get("Node Type").and_then(Value::as_str) {
            Some("ModifyTable") => v.get("Operation").and_then(Value::as_str).unwrap_or("ModifyTable").to_string(),
            other => other.unwrap_or("?").to_string(),
        },
        object,
        detail,
        est_rows: est,
        act_rows: act,
        cost: if analyzed { None } else { num(v, "Total Cost").map(|c| (c - child_cost).max(0.0)) },
        time_ms: if analyzed { pg_incl_time(v).map(|t| (t - child_time).max(0.0)) } else { None },
        pct: 0.0,
        warn,
        children: kids.iter().map(|c| pg_node(c, analyzed)).collect(),
        id: None,
        summary: None,
    }
}

/// Distribui `pct` pelo peso de cada nó (tempo no real, custo/bytes no estimado).
fn fill_pct(root: &mut PlanNode, weight: fn(&PlanNode) -> f64) {
    fn total(n: &PlanNode, w: fn(&PlanNode) -> f64) -> f64 {
        w(n) + n.children.iter().map(|c| total(c, w)).sum::<f64>()
    }
    fn set(n: &mut PlanNode, w: fn(&PlanNode) -> f64, t: f64) {
        n.pct = if t > 0.0 { 100.0 * w(n) / t } else { 0.0 };
        n.children.iter_mut().for_each(|c| set(c, w, t));
    }
    let t = total(root, weight);
    set(root, weight, t);
}

pub fn parse_pg(json: &str, analyzed: bool) -> Result<Plan, DbError> {
    let v: Value = serde_json::from_str(json).map_err(bad_json)?;
    let top = v.get(0).ok_or_else(|| bad_json("[]"))?;
    let plan = top.get("Plan").ok_or_else(|| bad_json("Plan"))?;
    let mut root = pg_node(plan, analyzed);
    if analyzed {
        fill_pct(&mut root, |n| n.time_ms.unwrap_or(0.0));
    } else {
        fill_pct(&mut root, |n| n.cost.unwrap_or(0.0));
    }
    let total_ms = if analyzed { num(top, "Execution Time").or_else(|| pg_incl_time(plan)) } else { None };
    Ok(Plan { root, analyzed, total_ms, note: None, profile: None })
}

/// Monta a árvore a partir de (id, pai, nó); um nó com vários pais fica sob o primeiro.
fn tree_from_parents(mut items: Vec<(i64, Option<i64>, PlanNode)>) -> Result<PlanNode, DbError> {
    fn take(id: i64, items: &mut Vec<(i64, Option<i64>, PlanNode)>) -> Option<PlanNode> {
        let ix = items.iter().position(|(i, _, _)| *i == id)?;
        let (_, _, mut node) = items.remove(ix);
        let kids: Vec<i64> = items.iter().filter(|(_, p, _)| *p == Some(id)).map(|(i, _, _)| *i).collect();
        node.children = kids.into_iter().filter_map(|k| take(k, items)).collect();
        Some(node)
    }
    let root = items.iter().find(|(_, p, _)| p.is_none()).map(|(i, _, _)| *i).ok_or_else(|| bad_json("root"))?;
    take(root, &mut items).ok_or_else(|| bad_json("root"))
}

fn first_parent(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_i64)
}

/// Leu mais de 90% das partições de uma tabela com mais de 100: o filtro não podou.
fn bad_pruning(scanned: f64, total: f64) -> bool {
    total > 100.0 && scanned / total > 0.9
}

fn pruning_warn(scanned: f64, total: f64) -> Option<String> {
    bad_pruning(scanned, total).then(|| t!("plan.warn_pruning", a = short_count(scanned), t = short_count(total)).to_string())
}

/// `EXPLAIN USING JSON`: operadores ligados por `parentOperators`; o peso são os bytes lidos.
pub fn parse_sf_explain(json: &str) -> Result<Plan, DbError> {
    let v: Value = serde_json::from_str(json).map_err(bad_json)?;
    let steps = v.get("Operations").and_then(Value::as_array).ok_or_else(|| bad_json("Operations"))?;
    let items = steps
        .iter()
        .flat_map(|step| step.as_array().into_iter().flatten())
        .map(|o| {
            let mut detail: Vec<(String, String)> = o.get("expressions").and_then(Value::as_array).into_iter().flatten().map(|e| ("expression".to_string(), text(e))).collect();
            let (pa, pt, bytes) = (num(o, "partitionsAssigned"), num(o, "partitionsTotal"), num(o, "bytesAssigned"));
            if let (Some(a), Some(t)) = (pa, pt) {
                detail.push(("partitions".into(), format!("{} / {}", short_count(a), short_count(t))));
            }
            if let Some(b) = bytes {
                detail.push(("bytes".into(), short_count(b)));
            }
            let node = PlanNode {
                op: o.get("operation").and_then(Value::as_str).unwrap_or("?").into(),
                object: o.get("objects").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str).map(str::to_string),
                detail,
                cost: bytes,
                warn: pa.zip(pt).and_then(|(a, t)| pruning_warn(a, t)).into_iter().collect(),
                ..Default::default()
            };
            (o.get("id").and_then(Value::as_i64).unwrap_or(-1), first_parent(o.get("parentOperators")), node)
        })
        .collect();
    let mut root = tree_from_parents(items)?;
    fill_pct(&mut root, |n| n.cost.unwrap_or(0.0));
    let note = no_cost_note(&root);
    Ok(Plan { root, analyzed: false, total_ms: None, note, profile: None })
}

/// Estatísticas reais por operador de uma query já executada (o id vem do Snowflake; sem aspas por garantia).
/// As colunas VARIANT vêm como texto: o conector recusa o `NaN` que o Snowflake põe no JSON.
pub fn operator_stats_sql(query_id: &str) -> String {
    format!(
        "SELECT operator_id, parent_operators::string AS parent_operators, operator_type, operator_statistics::string AS operator_statistics, \
execution_time_breakdown::string AS execution_time_breakdown, operator_attributes::string AS operator_attributes \
FROM TABLE(GET_QUERY_OPERATOR_STATS('{}'))",
        query_id.replace('\'', "")
    )
}

/// `NaN` / `Infinity` / `-Infinity` soltos (fora de strings) viram `null`: JSON não tem esses valores.
pub fn sanitize_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let (mut in_str, mut esc) = (false, false);
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            rest = &rest[c.len_utf8()..];
            continue;
        }
        in_str = c == '"';
        match ["-Infinity", "Infinity", "NaN"].into_iter().find(|t| rest.starts_with(t)) {
            Some(t) => {
                out.push_str("null");
                rest = &rest[t.len()..];
            }
            None => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

/// `GET_QUERY_OPERATOR_STATS`: colunas pelo nome; variants chegam como texto JSON.
pub fn parse_sf_stats(names: &[String], rows: &[Vec<Option<String>>]) -> Result<Plan, DbError> {
    if rows.is_empty() {
        let root = PlanNode { op: "Result".into(), ..Default::default() };
        return Ok(Plan { root, analyzed: true, total_ms: None, note: Some(PlanNote::Cached), profile: None });
    }
    let col = |n: &str| names.iter().position(|c| c.eq_ignore_ascii_case(n)).ok_or_else(|| bad_json(n));
    let (id, parents, op, stats, time, attrs) = (col("OPERATOR_ID")?, col("PARENT_OPERATORS")?, col("OPERATOR_TYPE")?, col("OPERATOR_STATISTICS")?, col("EXECUTION_TIME_BREAKDOWN")?, col("OPERATOR_ATTRIBUTES")?);
    let cell = |r: &[Option<String>], i: usize| r.get(i).cloned().flatten();
    let json = |r: &[Option<String>], i: usize| cell(r, i).and_then(|s| serde_json::from_str::<Value>(&sanitize_json(&s)).ok()).unwrap_or(Value::Null);
    let mut prof = Profile::default();
    let mut breakdown: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();
    let (mut cache_w, mut cache_seen) = (0.0, false);
    let items = rows
        .iter()
        .map(|r| {
            let (st, tm, at) = (json(r, stats), json(r, time), json(r, attrs));
            let f = |v: &Value, p: &str| v.pointer(p).and_then(Value::as_f64).unwrap_or(0.0);
            // Perfil: soma das categorias de tempo e das estatísticas de leitura de todos os operadores.
            if let Some(obj) = tm.as_object() {
                for (k, v) in obj.iter().filter(|(k, _)| k.as_str() != "overall_percentage") {
                    *breakdown.entry(k.clone()).or_insert(0.0) += v.as_f64().unwrap_or(0.0);
                }
            }
            let bytes = f(&st, "/io/bytes_scanned");
            prof.bytes_scanned += bytes;
            if let Some(c) = st.pointer("/io/percentage_scanned_from_cache").and_then(Value::as_f64) {
                cache_w += c * bytes;
                cache_seen = true;
            }
            if let Some(p) = st.pointer("/io/scan_progress").and_then(Value::as_f64) {
                prof.scan_progress = Some(prof.scan_progress.map_or(p, |m: f64| m.min(p)));
            }
            prof.partitions_scanned += f(&st, "/pruning/partitions_scanned");
            prof.partitions_total += f(&st, "/pruning/partitions_total");
            prof.spilled_local += f(&st, "/spilling/bytes_spilled_local_storage");
            prof.spilled_remote += f(&st, "/spilling/bytes_spilled_remote_storage");
            prof.network_bytes += f(&st, "/network/network_bytes");
            let mut warn = Vec::new();
            if st.pointer("/spilling/bytes_spilled_remote_storage").and_then(Value::as_f64).is_some_and(|b| b > 0.0) {
                warn.push(t!("plan.warn_spill").to_string());
            }
            if let (Some(sc), Some(tt)) = (st.pointer("/pruning/partitions_scanned").and_then(Value::as_f64), st.pointer("/pruning/partitions_total").and_then(Value::as_f64)) {
                warn.extend(pruning_warn(sc, tt));
            }
            let mut detail: Vec<(String, String)> = at.as_object().into_iter().flatten().filter(|(k, _)| k.as_str() != "table_name").map(|(k, v)| (k.clone(), text(v))).collect();
            detail.extend(st.as_object().into_iter().flatten().map(|(k, v)| (k.clone(), text(v))));
            let node = PlanNode {
                op: cell(r, op).unwrap_or_default(),
                object: at.get("table_name").and_then(Value::as_str).map(str::to_string),
                detail,
                act_rows: st.get("output_rows").and_then(Value::as_f64),
                pct: tm.get("overall_percentage").and_then(Value::as_f64).unwrap_or(0.0) * 100.0,
                warn,
                id: cell(r, id).and_then(|s| s.parse().ok()),
                summary: summary_of(&at),
                ..Default::default()
            };
            let parent = first_parent(Some(&json(r, parents)));
            (cell(r, id).and_then(|s| s.parse().ok()).unwrap_or(-1), parent, node)
        })
        .collect();
    let root = tree_from_parents(items)?;
    let note = no_cost_note(&root);
    prof.from_cache = (cache_seen && prof.bytes_scanned > 0.0).then(|| cache_w / prof.bytes_scanned);
    prof.breakdown = breakdown.into_iter().filter(|(_, v)| *v > 0.0).collect();
    prof.breakdown.sort_by(|a, b| b.1.total_cmp(&a.1));
    Ok(Plan { root, analyzed: true, total_ms: None, note, profile: Some(prof) })
}

/// Raiz em cima; cada subárvore ocupa a soma das larguras dos filhos e o pai fica centralizado.
pub fn layout(root: &PlanNode, box_w: f32, box_h: f32, gap_x: f32, gap_y: f32) -> Vec<Placed> {
    fn width(n: &PlanNode, w: f32, g: f32) -> f32 {
        if n.children.is_empty() { w } else { n.children.iter().map(|c| width(c, w, g)).sum::<f32>() + g * (n.children.len() - 1) as f32 }
    }
    fn go(n: &PlanNode, path: Vec<usize>, left: f32, depth: usize, parent: Option<usize>, dims: (f32, f32, f32, f32), out: &mut Vec<Placed>) {
        let (w, h, gx, gy) = dims;
        let me = out.len();
        out.push(Placed { path: path.clone(), x: left + (width(n, w, gx) - w) / 2.0, y: depth as f32 * (h + gy), parent });
        let mut x = left;
        for (i, c) in n.children.iter().enumerate() {
            let mut p = path.clone();
            p.push(i);
            go(c, p, x, depth + 1, Some(me), dims, out);
            x += width(c, w, gx) + gx;
        }
    }
    let mut out = Vec::new();
    go(root, Vec::new(), 0.0, 0, None, (box_w, box_h, gap_x, gap_y), &mut out);
    out
}

pub fn node_at<'a>(root: &'a PlanNode, path: &[usize]) -> Option<&'a PlanNode> {
    path.iter().try_fold(root, |n, &i| n.children.get(i))
}

/// Caminho do nó de maior `pct` (o diagrama começa com ele selecionado).
pub fn hottest(root: &PlanNode) -> Vec<usize> {
    fn go(n: &PlanNode, path: &mut Vec<usize>, best: &mut (f64, Vec<usize>)) {
        if n.pct > best.0 {
            *best = (n.pct, path.clone());
        }
        for (i, c) in n.children.iter().enumerate() {
            path.push(i);
            go(c, path, best);
            path.pop();
        }
    }
    let mut best = (f64::MIN, Vec::new());
    go(root, &mut Vec::new(), &mut best);
    best.1
}

/// Espessura da seta pelas linhas que passam: 1 + log10(linhas + 1), no máximo 8.
pub fn edge_width(rows: Option<f64>) -> f32 {
    rows.map_or(1.0, |r| (1.0 + (r.max(0.0) + 1.0).log10()).min(8.0) as f32)
}

pub fn short_count(n: f64) -> String {
    if n >= 1e6 {
        format!("{:.1}M", n / 1e6)
    } else if n >= 1e3 {
        format!("{:.0}k", n / 1e3)
    } else {
        format!("{n:.0}")
    }
}

#[cfg(test)]
mod tests {
    fn stats_row(id: &str, parent: Option<&str>, op: &str, stats: &str, time: &str, attrs: &str) -> Vec<Option<String>> {
        vec![Some(id.into()), parent.map(String::from), Some(op.into()), Some(stats.into()), Some(time.into()), Some(attrs.into())]
    }

    fn stats_names() -> Vec<String> {
        ["OPERATOR_ID", "PARENT_OPERATORS", "OPERATOR_TYPE", "OPERATOR_STATISTICS", "EXECUTION_TIME_BREAKDOWN", "OPERATOR_ATTRIBUTES"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn profile_aggregates_time_and_scan_statistics() {
        let rows = vec![
            stats_row("0", None, "Result", r#"{"output_rows": 10}"#, r#"{"overall_percentage": 0.1, "processing": 0.1}"#, "{}"),
            stats_row("1", Some("[0]"), "Join", r#"{"output_rows": 159400}"#, r#"{"overall_percentage": 0.5, "processing": 0.4, "synchronization": 0.1}"#, r#"{"equality_join_condition": "(F.A = T.B)"}"#),
            stats_row(
                "2",
                Some("[1]"),
                "TableScan",
                r#"{"output_rows": 56920, "io": {"bytes_scanned": 300, "percentage_scanned_from_cache": 1.0, "scan_progress": 1.0}, "pruning": {"partitions_scanned": 374, "partitions_total": 393}, "spilling": {"bytes_spilled_local_storage": 7}}"#,
                r#"{"overall_percentage": 0.4, "processing": 0.3, "initialization": 0.1}"#,
                r#"{"table_name": "DB.S.T"}"#,
            ),
            stats_row("3", Some("[1]"), "TableScan", r#"{"io": {"bytes_scanned": 100, "percentage_scanned_from_cache": 0.0, "scan_progress": 0.5}}"#, "{}", r#"{"table_name": "DB.S.U"}"#),
        ];
        let plan = parse_sf_stats(&stats_names(), &rows).unwrap();
        let p = plan.profile.expect("perfil no plano real");
        let get = |k: &str| p.breakdown.iter().find(|(c, _)| c == k).map(|(_, v)| *v).unwrap_or(0.0);
        assert!((get("processing") - 0.8).abs() < 1e-9 && (get("synchronization") - 0.1).abs() < 1e-9 && (get("initialization") - 0.1).abs() < 1e-9);
        assert_eq!(p.breakdown.first().map(|(c, _)| c.as_str()), Some("processing"), "maior primeiro");
        assert_eq!(p.bytes_scanned, 400.0);
        assert!((p.from_cache.unwrap() - 0.75).abs() < 1e-9, "média ponderada pelos bytes");
        assert_eq!(p.scan_progress, Some(0.5), "o scan mais atrasado");
        assert_eq!((p.partitions_scanned, p.partitions_total), (374.0, 393.0));
        assert_eq!(p.spilled_local, 7.0);
        let join = &plan.root.children[0];
        assert_eq!((join.id, join.summary.as_deref()), (Some(1), Some("(F.A = T.B)")));
        assert_eq!(join.children[0].summary.as_deref(), Some("DB.S.T"));
        let top: Vec<i64> = top_nodes(&plan.root, 2).iter().filter_map(|p| node_at(&plan.root, p).and_then(|n| n.id)).collect();
        assert_eq!(top, vec![1, 2]);
    }

    #[test]
    fn query_timing_from_history() {
        assert!(query_timing_sql("a'b").contains("query_id = 'ab'"));
        let names: Vec<String> = ["COMPILATION_TIME", "QUEUED", "EXECUTION_TIME", "TOTAL_ELAPSED_TIME"].iter().map(|s| s.to_string()).collect();
        let t = parse_timing(&names, &[vec![Some("120".into()), Some("0".into()), Some("980".into()), Some("1110".into())]]).unwrap();
        assert_eq!(t, Timing { compile_ms: 120.0, queued_ms: 0.0, exec_ms: 980.0, total_ms: 1110.0 });
        assert_eq!(parse_timing(&names, &[]), None);
    }

    #[test]
    fn plan_without_any_cost_says_why() {
        let json = r#"{"Operations":[[{"id":0,"operation":"Result"},{"id":1,"operation":"DynamicSecureView","objects":["V (+ RowAccessPolicy)"],"parentOperators":[0]}]]}"#;
        assert_eq!(parse_sf_explain(json).unwrap().note, Some(PlanNote::NoCost { secure_views: true }));
        let with_bytes = r#"{"Operations":[[{"id":0,"operation":"Result"},{"id":1,"operation":"TableScan","bytesAssigned":100,"parentOperators":[0]}]]}"#;
        assert_eq!(parse_sf_explain(with_bytes).unwrap().note, None);
    }

    #[test]
    fn empty_operator_stats_mean_cached_result() {
        let names: Vec<String> = ["OPERATOR_ID", "PARENT_OPERATORS", "OPERATOR_TYPE", "OPERATOR_STATISTICS", "EXECUTION_TIME_BREAKDOWN", "OPERATOR_ATTRIBUTES"].iter().map(|s| s.to_string()).collect();
        let plan = parse_sf_stats(&names, &[]).unwrap();
        assert_eq!(plan.note, Some(PlanNote::Cached));
        assert!(plan.root.children.is_empty());
    }

    #[test]
    fn operator_stats_sql_quotes_the_id_safely() {
        let sql = operator_stats_sql("01b2-x");
        assert!(sql.contains("GET_QUERY_OPERATOR_STATS('01b2-x')"), "{sql}");
        assert!(sql.contains("execution_time_breakdown::string"), "variant chega como texto: o conector não aceita NaN");
        assert!(operator_stats_sql("a'b").contains("STATS('ab')"));
    }

    #[test]
    fn nan_in_stats_does_not_break_the_plan() {
        assert_eq!(sanitize_json("{\"other\": NaN, \"x\": -Infinity, \"s\": \"NaN\"}"), "{\"other\": null, \"x\": null, \"s\": \"NaN\"}");
        assert_eq!(sanitize_json(r#"{"a": "q\"NaN", "b": NaN}"#), r#"{"a": "q\"NaN", "b": null}"#, "aspas escapadas não fecham a string");
        let names: Vec<String> = ["OPERATOR_ID", "PARENT_OPERATORS", "OPERATOR_TYPE", "OPERATOR_STATISTICS", "EXECUTION_TIME_BREAKDOWN", "OPERATOR_ATTRIBUTES"].iter().map(|s| s.to_string()).collect();
        let row = vec![Some("0".into()), None, Some("Result".into()), Some("{}".into()), Some("{\n  \"other\": NaN,\n  \"overall_percentage\": 1.000000000000000e+00\n}".into()), Some("{}".into())];
        let plan = parse_sf_stats(&names, &[row]).unwrap();
        assert_eq!(plan.root.pct, 100.0);
    }

    use super::*;

    const ANALYZE: &str = include_str!("../tests/fixtures/pg_analyze.json");
    const ESTIMATE: &str = include_str!("../tests/fixtures/pg_estimate.json");

    fn leaf(op: &str) -> PlanNode {
        PlanNode { op: op.into(), ..Default::default() }
    }

    #[test]
    fn pg_analyze_builds_tree_with_exclusive_times() {
        let p = parse_pg(ANALYZE, true).unwrap();
        assert!(p.analyzed && p.total_ms.is_some());
        assert_eq!(p.root.op, "Aggregate");
        assert_eq!(p.root.children[0].op, "Hash Join");
        assert!(p.root.children[0].detail.iter().any(|(k, v)| k == "Hash Cond" && v.contains("a.x = b.y")));
        fn walk(n: &PlanNode, sum: &mut f64) {
            assert!(n.time_ms.unwrap() >= 0.0, "{}", n.op);
            *sum += n.pct;
            n.children.iter().for_each(|c| walk(c, sum));
        }
        let mut sum = 0.0;
        walk(&p.root, &mut sum);
        assert!((sum - 100.0).abs() < 0.5, "{sum}");
    }

    #[test]
    fn pg_estimate_uses_exclusive_cost() {
        let p = parse_pg(ESTIMATE, false).unwrap();
        assert!(!p.analyzed && p.total_ms.is_none());
        assert!(p.root.cost.unwrap() >= 0.0 && p.root.act_rows.is_none());
        assert_eq!(p.root.children[0].children[0].object.as_deref(), Some("generate_series"));
    }

    #[test]
    fn loops_multiply_time_and_never_go_negative() {
        let json = r#"[{"Plan":{"Node Type":"Nested Loop","Actual Total Time":10.0,"Actual Loops":1,"Actual Rows":5,"Plan Rows":5,"Total Cost":50,
            "Plans":[{"Node Type":"Index Scan","Relation Name":"t","Index Name":"t_pk","Actual Total Time":0.5,"Actual Loops":30,"Actual Rows":1,"Plan Rows":1,"Total Cost":1}]}}]"#;
        let p = parse_pg(json, true).unwrap();
        let scan = &p.root.children[0];
        assert_eq!(scan.time_ms, Some(15.0));
        assert_eq!(p.root.time_ms, Some(0.0), "pai menor que a soma dos filhos vira 0, não negativo");
        assert_eq!(scan.object.as_deref(), Some("t (t_pk)"));
        assert_eq!(scan.act_rows, Some(30.0), "linhas × loops");
    }

    #[test]
    fn pg_warnings_for_bad_estimates_and_wasteful_seq_scans() {
        let json = r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"big","Actual Total Time":5,"Actual Loops":1,"Actual Rows":10,"Plan Rows":1000,"Rows Removed by Filter":5000,"Total Cost":9}}]"#;
        let w = parse_pg(json, true).unwrap().root.warn;
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(parse_pg("{}", true).is_err() && parse_pg("nada", false).is_err());
    }

    #[test]
    fn sf_explain_links_operations_by_parent() {
        let p = parse_sf_explain(include_str!("../tests/fixtures/sf_explain.json")).unwrap();
        assert!(!p.analyzed);
        assert_eq!(p.root.op, "Result");
        let join = &p.root.children[0].children[0];
        assert_eq!(join.op, "InnerJoin");
        assert_eq!(join.children.len(), 2);
        let orders = &join.children[1].children[0];
        assert_eq!(orders.object.as_deref(), Some("DB.PUBLIC.ORDERS"));
        assert!(orders.pct > 90.0, "bytes do scan / bytes totais: {}", orders.pct);
        assert_eq!(orders.warn.len(), 1, "110 partições, todas lidas: poda ruim");
        assert!(join.children[0].warn.is_empty(), "10 partições: abaixo do limite");
    }

    #[test]
    fn sf_stats_use_time_share_and_warn_on_spill() {
        let v: Value = serde_json::from_str(include_str!("../tests/fixtures/sf_stats.json")).unwrap();
        let names: Vec<String> = v["names"].as_array().unwrap().iter().map(|n| n.as_str().unwrap().to_string()).collect();
        let rows: Vec<Vec<Option<String>>> = v["rows"].as_array().unwrap().iter().map(|r| r.as_array().unwrap().iter().map(|c| c.as_str().map(str::to_string)).collect()).collect();
        let p = parse_sf_stats(&names, &rows).unwrap();
        assert!(p.analyzed);
        let join = &p.root.children[0].children[0];
        assert!(join.detail.iter().any(|(k, _)| k == "equality_join_condition"));
        let orders = &join.children[1];
        assert_eq!(orders.object.as_deref(), Some("DB.PUBLIC.ORDERS"));
        assert!((orders.pct - 65.0).abs() < 0.01);
        assert_eq!(orders.act_rows, Some(990000.0));
        assert_eq!(orders.warn.len(), 2, "spill remoto e poda ruim: {:?}", orders.warn);
        assert!(parse_sf_stats(&names[..2], &rows).is_err(), "coluna faltando");
    }

    #[test]
    fn pg_modify_table_shows_the_operation() {
        let json = r#"[{"Plan":{"Node Type":"ModifyTable","Operation":"Update","Relation Name":"t","Total Cost":3}}]"#;
        assert_eq!(parse_pg(json, false).unwrap().root.op, "Update");
    }

    #[test]
    fn layout_centers_parents_and_never_overlaps() {
        let mut root = leaf("root");
        let mut mid = leaf("mid");
        mid.children = vec![leaf("a"), leaf("b")];
        root.children = vec![mid, leaf("c")];
        let placed = layout(&root, 100.0, 50.0, 20.0, 30.0);
        assert_eq!(placed.len(), 5);
        let at = |p: &[usize]| placed.iter().find(|x| x.path == p).unwrap();
        assert_eq!(at(&[0, 0]).x, 0.0);
        assert_eq!(at(&[0, 1]).x, 120.0);
        assert_eq!(at(&[0]).x, 60.0, "centralizado sobre a e b");
        assert_eq!(at(&[1]).x, 240.0);
        assert_eq!(at(&[]).x, 120.0, "centralizado entre mid e c");
        assert_eq!(at(&[0, 0]).y, 160.0);
        assert_eq!(at(&[0, 0]).parent, Some(placed.iter().position(|x| x.path == [0]).unwrap()));
        for (i, a) in placed.iter().enumerate() {
            for b in &placed[i + 1..] {
                assert!(a.y != b.y || (a.x - b.x).abs() >= 100.0, "{a:?} {b:?}");
            }
        }
    }

    #[test]
    fn helpers_for_the_diagram() {
        assert_eq!(edge_width(None), 1.0);
        assert_eq!(edge_width(Some(9.0)), 2.0);
        assert_eq!(edge_width(Some(1e12)), 8.0);
        assert_eq!(short_count(1_234_567.0), "1.2M");
        assert_eq!(short_count(98_000.0), "98k");
        assert_eq!(short_count(12.0), "12");
        let mut r = leaf("r");
        let mut hot = leaf("hot");
        hot.pct = 80.0;
        r.children = vec![leaf("x"), hot];
        assert_eq!(hottest(&r), vec![1]);
        assert_eq!(node_at(&r, &[1]).unwrap().op, "hot");
        assert!(node_at(&r, &[5]).is_none());
    }
}
