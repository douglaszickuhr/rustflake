//! Snowflake tasks: what one is (`SHOW TASKS`), its recent runs (`TASK_HISTORY`) and the task graph
//! it belongs to (predecessors), laid out in layers for drawing.

use std::collections::{HashMap, HashSet};

use crate::lexer::{qualify, sql_str};
use crate::snowflake::{Rows, named};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskInfo {
    pub db: String,
    pub schema: String,
    pub name: String,
    /// `started` or `suspended`.
    pub state: String,
    /// `60 MINUTE`, `USING CRON 0 7 * * * UTC`; None for a child task (it runs after its predecessors).
    pub schedule: Option<String>,
    /// None = serverless.
    pub warehouse: Option<String>,
    pub owner: Option<String>,
    pub comment: Option<String>,
    pub definition: String,
    pub condition: Option<String>,
    /// Fully qualified `DB.SCHEMA.NAME` of the tasks this one runs after.
    pub predecessors: Vec<String>,
}

impl TaskInfo {
    pub fn fq(&self) -> String {
        format!("{}.{}.{}", self.db, self.schema, self.name)
    }
}

/// Every task of the database: the graph may cross schemas, and the tree's listing comes from here too.
pub fn tasks_in_database_sql(db: &str) -> String {
    format!("show tasks in database {}", qualify(&[db]))
}

pub fn tasks_in_schema_sql(db: &str, schema: &str) -> String {
    format!("show tasks in schema {}", qualify(&[db, schema]))
}

/// `"DB"."S"."T"` or `DB.S.T` → `DB.S.T`.
fn unquote(fq: &str) -> String {
    fq.replace('"', "")
}

pub fn parse_tasks(names: &[String], rows: &Rows) -> Vec<TaskInfo> {
    named(names, rows)
        .filter_map(|r| {
            let preds = r.text("predecessors").and_then(|p| serde_json::from_str::<Vec<String>>(&p).ok()).unwrap_or_default();
            Some(TaskInfo {
                db: r.text("database_name")?,
                schema: r.text("schema_name")?,
                name: r.text("name")?,
                state: r.text("state").unwrap_or_default(),
                schedule: r.text("schedule").filter(|s| !s.trim().is_empty()),
                warehouse: r.text("warehouse").filter(|s| !s.trim().is_empty()),
                owner: r.text("owner"),
                comment: r.text("comment").filter(|s| !s.trim().is_empty()),
                definition: r.text("definition").unwrap_or_default(),
                condition: r.text("condition").filter(|s| !s.trim().is_empty()),
                predecessors: preds.iter().map(|p| unquote(p)).collect(),
            })
        })
        .collect()
}

/// How often it runs, as people say it: "every 60 min", "cron 0 7 * * * UTC", "after LOAD_A, LOAD_B".
pub fn schedule_label(t: &TaskInfo) -> String {
    match t.schedule.as_deref().map(str::trim) {
        Some(s) if s.to_uppercase().starts_with("USING CRON") => format!("cron {}", s[10..].trim()),
        Some(s) => match s.split_once(' ') {
            Some((n, unit)) if unit.to_uppercase().starts_with("MINUTE") => format!("every {n} min"),
            Some((n, unit)) if unit.to_uppercase().starts_with("HOUR") => format!("every {n} h"),
            Some((n, unit)) if unit.to_uppercase().starts_with("SECOND") => format!("every {n} s"),
            _ => s.to_string(),
        },
        None if !t.predecessors.is_empty() => format!("after {}", t.predecessors.iter().map(|p| p.rsplit('.').next().unwrap_or(p)).collect::<Vec<_>>().join(", ")),
        None => "not scheduled".into(),
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskRun {
    /// `SCHEDULED`, `EXECUTING`, `SUCCEEDED`, `FAILED`, `CANCELLED`, `SKIPPED`.
    pub state: String,
    pub scheduled: Option<String>,
    pub started: Option<String>,
    pub completed: Option<String>,
    pub error: Option<String>,
    pub query_id: Option<String>,
}

/// The task's runs over the last `days` days, newest first. `task_name` takes the bare name; the
/// rows are filtered to this task's schema (the same name may exist in another schema).
pub fn runs_sql(db: &str, schema: &str, name: &str, days: u32) -> String {
    format!(
        "select state, scheduled_time, query_start_time, completed_time, error_code, error_message, query_id from table({}.information_schema.task_history(\
         scheduled_time_range_start => dateadd('day', -{days}, current_timestamp()), result_limit => 500, task_name => {})) \
         where database_name = {} and schema_name = {} order by scheduled_time desc",
        qualify(&[db]),
        sql_str(name),
        sql_str(db),
        sql_str(schema)
    )
}

pub fn parse_runs(names: &[String], rows: &Rows) -> Vec<TaskRun> {
    named(names, rows)
        .map(|r| TaskRun {
            state: r.text("state").unwrap_or_default(),
            scheduled: r.text("scheduled_time"),
            started: r.text("query_start_time"),
            completed: r.text("completed_time"),
            error: match (r.text("error_code"), r.text("error_message")) {
                (Some(c), Some(m)) => Some(format!("{c}: {m}")),
                (None, Some(m)) => Some(m),
                (Some(c), None) => Some(c),
                (None, None) => None,
            },
            query_id: r.text("query_id"),
        })
        .collect()
}

/// Tables a task's runs touched, from `ACCESS_HISTORY`: written ones, and those only read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DataFlow {
    pub reads: Vec<String>,
    pub writes: Vec<String>,
}

/// Everything the runs touched. A task run's statements are logged as child queries under the run's
/// query id (`root_query_id`); `since` bounds the scan (`ACCOUNT_USAGE` is big).
pub fn data_flow_sql(run_ids: &[String], since: &str) -> String {
    let ids = run_ids.iter().map(|i| sql_str(i)).collect::<Vec<_>>().join(", ");
    let part = |col: &str, dir: &str| {
        format!(
            "select distinct o.value:\"objectName\"::string as name, o.value:\"objectDomain\"::string as domain, '{dir}' as dir \
             from snowflake.account_usage.access_history h, lateral flatten(input => h.{col}) o \
             where h.query_start_time >= dateadd('hour', -1, {}::timestamp_ltz) and (h.root_query_id in ({ids}) or h.query_id in ({ids}))",
            sql_str(since)
        )
    };
    format!("{} union {} union {}", part("direct_objects_accessed", "read"), part("base_objects_accessed", "read"), part("objects_modified", "write"))
}

pub fn parse_data_flow(names: &[String], rows: &Rows) -> DataFlow {
    let mut reads = std::collections::BTreeSet::new();
    let mut writes = std::collections::BTreeSet::new();
    for r in named(names, rows) {
        let (Some(name), Some(dir)) = (r.text("name"), r.text("dir")) else { continue };
        // RESULT_SCAN and other table functions aren't data that lives anywhere.
        if r.text("domain").is_some_and(|d| d.eq_ignore_ascii_case("Table function")) {
            continue;
        }
        if dir == "write" {
            writes.insert(name);
        } else {
            reads.insert(name);
        }
    }
    // Read and also written (a work table, or state read before it's updated): it's a write.
    let reads = reads.into_iter().filter(|r| !writes.contains(r)).collect();
    DataFlow { reads, writes: writes.into_iter().collect() }
}

/// The task graph `focus` belongs to (everything connected through predecessors), in layers: roots
/// first, each task one layer after its deepest predecessor.
pub fn graph_layers(all: &[TaskInfo], focus: &str) -> Vec<Vec<TaskInfo>> {
    let by_fq: HashMap<String, &TaskInfo> = all.iter().map(|t| (t.fq(), t)).collect();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for t in all {
        for p in &t.predecessors {
            children.entry(p.clone()).or_default().push(t.fq());
        }
    }
    // Connected component: walk both ways from the focus.
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack = vec![focus.to_string()];
    while let Some(fq) = stack.pop() {
        if !seen.insert(fq.clone()) {
            continue;
        }
        if let Some(t) = by_fq.get(&fq) {
            stack.extend(t.predecessors.iter().cloned());
        }
        stack.extend(children.get(&fq).cloned().unwrap_or_default());
    }
    // Depth = longest path from a root (predecessors outside the database count as roots).
    fn depth(fq: &str, by_fq: &HashMap<String, &TaskInfo>, memo: &mut HashMap<String, usize>, guard: usize) -> usize {
        if let Some(d) = memo.get(fq) {
            return *d;
        }
        let d = match by_fq.get(fq) {
            Some(t) if guard < 64 => t.predecessors.iter().filter(|p| by_fq.contains_key(*p)).map(|p| depth(p, by_fq, memo, guard + 1) + 1).max().unwrap_or(0),
            _ => 0,
        };
        memo.insert(fq.to_string(), d);
        d
    }
    let mut memo = HashMap::new();
    let mut layers: Vec<Vec<TaskInfo>> = Vec::new();
    let mut members: Vec<&TaskInfo> = seen.iter().filter_map(|fq| by_fq.get(fq).copied()).collect();
    members.sort_by(|a, b| a.name.cmp(&b.name));
    for t in members {
        let d = depth(&t.fq(), &by_fq, &mut memo, 0);
        if layers.len() <= d {
            layers.resize_with(d + 1, Vec::new);
        }
        layers[d].push(t.clone());
    }
    layers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str, preds: &[&str]) -> TaskInfo {
        TaskInfo { db: "DB".into(), schema: "S".into(), name: name.into(), predecessors: preds.iter().map(|p| format!("DB.S.{p}")).collect(), ..Default::default() }
    }

    #[test]
    fn show_tasks_rows_become_task_info() {
        let names: Vec<String> = ["database_name", "schema_name", "name", "state", "schedule", "warehouse", "definition", "predecessors"].map(String::from).into();
        let rows = vec![vec![
            Some("DB".into()),
            Some("S".into()),
            Some("CHILD".into()),
            Some("started".into()),
            None,
            Some("".into()),
            Some("insert into x select 1".into()),
            Some("[\n  \"\\\"DB\\\".\\\"S\\\".\\\"ROOT\\\"\"\n]".into()),
        ]];
        let got = parse_tasks(&names, &rows);
        assert_eq!((got[0].predecessors.as_slice(), got[0].warehouse.as_deref(), got[0].schedule.as_deref()), (&["DB.S.ROOT".to_string()][..], None, None));
    }

    #[test]
    fn schedules_read_like_people_say_them() {
        let with = |s: Option<&str>, preds: &[&str]| schedule_label(&TaskInfo { schedule: s.map(String::from), ..t("X", preds) });
        assert_eq!(with(Some("60 MINUTE"), &[]), "every 60 min");
        assert_eq!(with(Some("USING CRON 0 7 * * * UTC"), &[]), "cron 0 7 * * * UTC");
        assert_eq!(with(None, &["LOAD_A", "LOAD_B"]), "after LOAD_A, LOAD_B");
        assert_eq!(with(None, &[]), "not scheduled");
    }

    #[test]
    fn graph_holds_the_focus_component_in_layers() {
        let all = [t("ROOT", &[]), t("A", &["ROOT"]), t("B", &["ROOT"]), t("FINAL", &["A", "B"]), t("OTHER_ROOT", &[]), t("OTHER", &["OTHER_ROOT"])];
        let names = |l: Vec<Vec<TaskInfo>>| l.into_iter().map(|x| x.into_iter().map(|t| t.name).collect::<Vec<_>>()).collect::<Vec<_>>();
        assert_eq!(names(graph_layers(&all, "DB.S.A")), [vec!["ROOT"], vec!["A", "B"], vec!["FINAL"]]);
        assert_eq!(names(graph_layers(&all, "DB.S.OTHER")), [vec!["OTHER_ROOT"], vec!["OTHER"]]);
        assert_eq!(names(graph_layers(&all, "DB.S.MISSING")), Vec::<Vec<String>>::new(), "unknown focus: nothing");
    }

    #[test]
    fn data_flow_keeps_writes_and_what_was_only_read() {
        let names: Vec<String> = ["name", "domain", "dir"].map(String::from).into();
        let row = |n: &str, d: &str, dir: &str| vec![Some(n.to_string()), Some(d.to_string()), Some(dir.to_string())];
        let rows = vec![
            row("RAW.INFORMATION_SCHEMA.TABLES", "View", "read"),
            row("OPS.S.TMP_SNAPSHOT", "Table", "write"),
            row("OPS.S.TMP_SNAPSHOT", "Table", "read"),
            row("OPS.S.STATE", "Table", "read"),
            row("OPS.S.STATE", "Table", "write"),
            row("RESULT_SCAN", "Table function", "read"),
        ];
        let f = parse_data_flow(&names, &rows);
        assert_eq!(f.reads, ["RAW.INFORMATION_SCHEMA.TABLES"]);
        assert_eq!(f.writes, ["OPS.S.STATE", "OPS.S.TMP_SNAPSHOT"]);
        let q = data_flow_sql(&["a".into(), "b".into()], "2026-10-21 19:00:00");
        assert!(q.contains("root_query_id in ('a', 'b')") && q.matches(" union ").count() == 2, "{q}");
    }

    #[test]
    fn runs_query_filters_to_the_tasks_own_schema() {
        let q = runs_sql("DB", "S", "o'k", 7);
        assert!(q.contains("task_name => 'o''k'") && q.contains("where database_name = 'DB' and schema_name = 'S'"), "{q}");
    }
}
