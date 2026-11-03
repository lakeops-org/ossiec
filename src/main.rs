use std::process::ExitCode;

const USAGE: &str = "\
ossiec — compile Apache Ossie semantic models to SQL

USAGE:
    ossiec validate <model.yaml>
    ossiec describe <model.yaml>
    ossiec compile  <model.yaml> <request.json | -> [--dialect <name>] [--prefer <OSSIE_DIALECT>] [--compact] [--json]
    ossiec compile-l2 <model.yaml> <query.sql | -> [--dialect <name>] [--prefer <OSSIE_DIALECT>] [--include-unmatched] [--compact] [--json]

OPTIONS:
    --dialect <name>   Target dialect: trino, snowflake, bigquery, duckdb, postgres, clickhouse,
                       starrocks, databricks, redshift, athena, mysql, ... (default: the model's source dialect)
    --prefer <name>    Prefer this Ossie dialect's expressions when a field or metric declares one
    --include-unmatched
                       Layer 2: also return one-side rows no fact reaches (same as a
                       request's includeUnmatched, or `-- ossiec: includeUnmatched` in the SQL)
    --compact          One-line SQL
    --json             Print the full compile result (SQL, dialect, warnings, hash) as JSON

    compile-l2         Ossie Layer 2: SQL with AGG(measure) (Appendix A.1-A.6, incl. derived tables A.2-A.3)
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
            let req: ossiec::Request =
                serde_json::from_str(&read(request)?).map_err(|e| format!("request: {e}"))?;
            print_compiled(
                ossiec::compile(&g, &req, &opts).map_err(|e| e.to_string())?,
                json,
            )
        }
        ["compile-l2", model, query] => {
            let g = graph(model)?;
            print_compiled(
                ossiec::compile_layer2(&g, &read(query)?, &opts).map_err(|e| e.to_string())?,
                json,
            )
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
