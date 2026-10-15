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
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub root: PlanNode,
    pub analyzed: bool,
    pub total_ms: Option<f64>,
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
        op: v.get("Node Type").and_then(Value::as_str).unwrap_or("?").to_string(),
        object,
        detail,
        est_rows: est,
        act_rows: act,
        cost: if analyzed { None } else { num(v, "Total Cost").map(|c| (c - child_cost).max(0.0)) },
        time_ms: if analyzed { pg_incl_time(v).map(|t| (t - child_time).max(0.0)) } else { None },
        pct: 0.0,
        warn,
        children: kids.iter().map(|c| pg_node(c, analyzed)).collect(),
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
    Ok(Plan { root, analyzed, total_ms })
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
    Ok(Plan { root, analyzed: false, total_ms: None })
}

/// `GET_QUERY_OPERATOR_STATS`: colunas pelo nome; variants chegam como texto JSON.
pub fn parse_sf_stats(names: &[String], rows: &[Vec<Option<String>>]) -> Result<Plan, DbError> {
    let col = |n: &str| names.iter().position(|c| c.eq_ignore_ascii_case(n)).ok_or_else(|| bad_json(n));
    let (id, parents, op, stats, time, attrs) = (col("OPERATOR_ID")?, col("PARENT_OPERATORS")?, col("OPERATOR_TYPE")?, col("OPERATOR_STATISTICS")?, col("EXECUTION_TIME_BREAKDOWN")?, col("OPERATOR_ATTRIBUTES")?);
    let cell = |r: &[Option<String>], i: usize| r.get(i).cloned().flatten();
    let json = |r: &[Option<String>], i: usize| cell(r, i).and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(Value::Null);
    let items = rows
        .iter()
        .map(|r| {
            let (st, tm, at) = (json(r, stats), json(r, time), json(r, attrs));
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
                ..Default::default()
            };
            let parent = first_parent(Some(&json(r, parents)));
            (cell(r, id).and_then(|s| s.parse().ok()).unwrap_or(-1), parent, node)
        })
        .collect();
    Ok(Plan { root: tree_from_parents(items)?, analyzed: true, total_ms: None })
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
