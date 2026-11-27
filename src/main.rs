use std::process::ExitCode;

const USAGE: &str = "\
ossiec — compile Apache Ossie semantic models to SQL

USAGE:
    ossiec validate <model.yaml>
    ossiec describe <model.yaml>
    ossiec compile     <model.yaml> <request.json | -> [--dialect <name>] [--prefer <OSSIE_DIALECT>] [--compact] [--json]
    ossiec compile-sql <model.yaml> <query.sql | -> [--dialect <name>] [--prefer <OSSIE_DIALECT>] [--include-unmatched] [--compact] [--json]
    ossiec persist-view <model.yaml> <view.sql> [--out <model.yaml>]

OPTIONS:
    --dialect <name>   Target dialect: trino, snowflake, bigquery, duckdb, postgres, clickhouse,
                       starrocks, spark, databricks, redshift, athena, mysql, ... (default: the model's source dialect)
    --prefer <name>    Prefer this Ossie dialect's expressions when a field or metric declares one
    --include-unmatched
                       Layer 2: also return one-side rows no fact reaches (same as a
                       request's includeUnmatched, or `-- ossiec: includeUnmatched` in the SQL)
    --out <path>       persist-view: write the updated model YAML to this file (default: stdout).
                       The model is re-serialized: unknown keys and comments are not preserved
    --compact          One-line SQL
    --json             Print the full compile result (SQL, dialect, warnings, hash) as JSON

    compile            Structured request (Ossie Layer 3 surface)
    compile-sql        Consumer SQL with AGG(measure) (Ossie Layer 2 surface); alias: compile-l2.
                       A script may define CREATE VIEW measure relations (A.7) and end with the
                       SELECT to compile. Views are session state until persist-view.
    persist-view       Write a CREATE VIEW's TO MEASURE items into the model as permanent
                       metrics; print the updated YAML (or --out <file>). Refuses a view
                       with WHERE, LIMIT/OFFSET or a join other than LEFT JOIN.
";

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn read(path: &str) -> Result<String, String> {
    if path == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).map_err(|e| e.to_string())?;
        return Ok(s);
    }
    std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

fn graph(path: &str) -> Result<ossiec::SemanticGraph, String> {
    let g = ossiec::from_yaml(&read(path)?).map_err(|e| e.to_string())?;
    for w in g.warnings() {
        eprintln!("warning: {w}");
    }
    Ok(g)
}

fn run(args: Vec<String>) -> Result<(), String> {
    let mut pos = vec![];
    let mut opts = ossiec::Options::default();
    let mut json = false;
    let mut out_path: Option<String> = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "--dialect" => {
                let d = it.next().ok_or("--dialect needs a value")?;
                opts.target = Some(d.parse().map_err(|_| format!("unknown dialect `{d}`"))?);
            }
            "--prefer" => opts.prefer_dialect = Some(it.next().ok_or("--prefer needs a value")?),
            "--include-unmatched" => opts.include_unmatched = true,
            "--out" => out_path = Some(it.next().ok_or("--out needs a value")?),
            "--compact" => opts.pretty = false,
            "--json" => json = true,
            _ => pos.push(a),
        }
    }
    match pos
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["validate", model] => {
            let g = graph(model)?;
            let c = g.describe();
            let blocked = c.metrics.iter().filter(|m| !m.queryable).count();
            println!(
                "ok: `{}` — {} datasets, {} relationships, {} metrics ({} not queryable yet)",
                c.name,
                g.model().datasets.len(),
                c.relationships.len(),
                c.metrics.len(),
                blocked
            );
            Ok(())
        }
        ["describe", model] => {
            let c = graph(model)?.describe();
            println!(
                "{}",
                serde_json::to_string_pretty(&c).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        ["compile", model, request] => {
            let g = graph(model)?;
            let req = ossiec::Request::from_json(&read(request)?).map_err(|e| e.to_string())?;
            print_compiled(
                ossiec::compile(&g, &req, &opts).map_err(|e| e.to_string())?,
                json,
            )
        }
        ["compile-sql" | "compile-l2", model, query] => {
            let g = graph(model)?;
            print_compiled(
                ossiec::compile_sql_script(&g, &read(query)?, &opts).map_err(|e| e.to_string())?,
                json,
            )
        }
        ["persist-view", model, view] => {
            let g = graph(model)?;
            let mut catalog = ossiec::Layer2Catalog::new();
            let name = ossiec::define_layer2_view(&g, &mut catalog, &read(view)?, &opts)
                .map_err(|e| e.to_string())?;
            let (g2, added) = ossiec::persist_layer2_view(&g, &catalog, &name, &opts)
                .map_err(|e| e.to_string())?;
            for m in &added {
                eprintln!("persisted: {m}");
            }
            let yaml = ossiec::to_yaml(g2.model()).map_err(|e| e.to_string())?;
            if let Some(path) = out_path {
                std::fs::write(&path, &yaml).map_err(|e| format!("{path}: {e}"))?;
                eprintln!("wrote {path}");
            } else {
                print!("{yaml}");
            }
            Ok(())
        }
        _ => Err(format!("unexpected arguments\n\n{USAGE}")),
    }
}

fn print_compiled(c: ossiec::Compiled, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&c).map_err(|e| e.to_string())?
        );
    } else {
        for w in &c.warnings {
            eprintln!("warning: {w}");
        }
        println!("{}", c.sql);
    }
    Ok(())
}
