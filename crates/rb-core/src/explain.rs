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
