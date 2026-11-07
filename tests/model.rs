//! Loading and binding: every vendored Ossie example, and one test per validation rule.

use ossiec::Error;

fn fixture_file(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/ossie/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[test]
fn every_ossie_example_model_binds() {
    for f in [
        "tpcds_semantic_model.yaml",
        "flights.semantic_model.yaml",
        "renamed_fields.yaml",
        "m0_ansi_qualified.ossie.yaml",
        "m0_ossiesql_qualified.ossie.yaml",
        "m0_sum_bare.ossie.yaml",
        "m1_scalar_field.ossie.yaml",
        "m2_aggregates_bare.ossie.yaml",
        "m2_aggregates_qualified.ossie.yaml",
        "m3_filtered_agg_bare.ossie.yaml",
        "m3_filtered_agg_qualified.ossie.yaml",
    ] {
        ossiec::from_yaml(&fixture_file(f)).unwrap_or_else(|e| panic!("{f}: {e}"));
    }
}

#[test]
fn every_compliance_metric_compiles() {
    for f in [
        "m0_ansi_qualified",
        "m0_ossiesql_qualified",
        "m0_sum_bare",
        "m1_scalar_field",
        "m2_aggregates_qualified",
        "m3_filtered_agg_qualified",
    ] {
        let g = ossiec::from_yaml(&fixture_file(&format!("{f}.ossie.yaml"))).unwrap();
        for m in g.describe().metrics {
            let req = ossiec::Request {
                metrics: vec![m.name.clone().into()],
                ..Default::default()
            };
            ossiec::compile(&g, &req, &Default::default())
                .unwrap_or_else(|e| panic!("{f}/{}: {e}", m.name));
        }
    }
}

#[test]
fn tpcds_window_metrics_compile_with_their_dimensions() {
    let g = ossiec::from_yaml(&fixture_file("tpcds_semantic_model.yaml")).unwrap();
    for m in g.describe().metrics {
        assert!(m.queryable, "{}", m.name);
        let req = ossiec::Request {
            metrics: vec![m.name.clone().into()],
            dimensions: m
                .requires_dimensions
                .iter()
                .map(|d| d.as_str().into())
                .collect(),
            deferred_metrics: m.requires_deferred_metrics,
            ..Default::default()
        };
        ossiec::compile(&g, &req, &Default::default())
            .unwrap_or_else(|e| panic!("{}: {e}", m.name));
    }
}

#[test]
fn ontology_and_legacy_documents_are_rejected_clearly() {
    let e = ossiec::load(&fixture_file("flights.yaml"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("ontology"), "{e}");
    let e = ossiec::load("semantic_model:\n  - name: x\n    datasets: []\n")
        .unwrap_err()
        .to_string();
    assert!(e.contains("pre-0.2"), "{e}");
}

fn invalid(yaml: &str) -> String {
    match ossiec::from_yaml(yaml) {
        Err(Error::InvalidModel(errs)) => errs.join("\n"),
        other => panic!("expected InvalidModel, got {:?}", other.map(|_| ())),
    }
}

const HEAD: &str = "version: \"0.2.0.dev0\"\nname: t\n";

#[test]
fn validation_rules() {
    let cases = [
        (
            "datasets:\n  - {name: a, source: t, fields: [{name: x, expression: {sql: x}}]}\n  - {name: A, source: t2}\n",
            "duplicate dataset `A`",
        ),
        (
            "datasets:\n  - {name: a, source: t, primary_key: [nope], fields: [{name: x, expression: {sql: x}}]}\n",
            "primary_key column `nope` is not a field",
        ),
        (
            "datasets:\n  - {name: a, source: t, fields: [{name: x, expression: {sql: SUM(x)}}]}\n",
            "field expressions are row-level",
        ),
        (
            "datasets:\n  - {name: a, source: t, fields: [{name: x, expression: {dialects: [{dialect: DAX, expression: 'SUM(x)'}]}}]}\n",
            "no expression in a SQL dialect ossiec can read (declared: DAX)",
        ),
        (
            "datasets:\n  - {name: a, source: t, fields: [{name: x, expression: {sql: x}}]}\nrelationships:\n  - {name: r, from: a, to: b, from_columns: [x], to_columns: [y]}\n",
            "`from` and `to` must name datasets",
        ),
        (
            "datasets:\n  - {name: a, source: t, fields: [{name: x, expression: {sql: x}}]}\n  - {name: b, source: u, fields: [{name: y, expression: {sql: y}}]}\nrelationships:\n  - {name: r, from: a, to: b, from_columns: [x], to_columns: [y, y]}\n",
            "same length",
        ),
        (
            "datasets:\n  - {name: a, source: t, fields: [{name: x, expression: {sql: x}}]}\nmetrics:\n  - {name: m, expression: {sql: 1 + 1}}\n",
            "a metric must aggregate",
        ),
        (
            "datasets:\n  - {name: a, source: t, fields: [{name: x, expression: {sql: x}}], metrics: [{name: m, expression: {sql: SUM(x) + x}}]}\n",
            "references a field outside an aggregate",
        ),
        (
            "datasets:\n  - {name: a, source: t, fields: [{name: x, expression: {sql: x}}]}\nmetrics:\n  - {name: m, expression: {sql: SUM(a.nope)}}\n",
            "`nope` is not a field of `a`",
        ),
        (
            "datasets:\n  - {name: a, source: t, fields: [{name: x, expression: {sql: x}}]}\n  - {name: b, source: u, fields: [{name: y, expression: {sql: y}}], metrics: [{name: m, expression: {sql: SUM(a.x)}}]}\n",
            "can only reference fields of its own dataset `b`",
        ),
    ];
    for (body, want) in cases {
        let got = invalid(&format!("{HEAD}{body}"));
        assert!(got.contains(want), "want `{want}` in:\n{got}");
    }
}

#[test]
fn identifiers_match_by_their_normalized_forms() {
    // Expression language, "Comparison Table": a regular name is upper-cased and a
    // quoted one kept, so `"AMOUNT"` matches `Amount` and `"Amount"` doesn't.
    let model = |r: &str| {
        format!(
            "{HEAD}datasets:\n  - {{name: Orders, source: t, fields: [{{name: Amount, expression: {{sql: amt}}}}, {{name: '\"Exact\"', expression: {{sql: e}}}}]}}\nmetrics:\n  - {{name: m, expression: {{sql: 'SUM({r})'}}}}\n"
        )
    };
    for ok in [
        "orders.AMOUNT",
        "ORDERS.amount",
        "\"ORDERS\".\"AMOUNT\"",
        "orders.\"Exact\"",
    ] {
        ossiec::from_yaml(&model(ok)).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    for bad in [
        "orders.\"Amount\"",
        "orders.\"amount\"",
        "orders.Exact",
        "orders.\"EXACT\"",
    ] {
        assert!(invalid(&model(bad)).contains("not a field"), "{bad}");
    }
    assert!(invalid(&model("\"Orders\".amount")).contains("unknown dataset"));
}

/// The Ossie compliance fixtures (M0-M3), executed: every metric compiled for
/// DuckDB and run against `samples.sales.orders`, whose NULL amount and NULL
/// customer exercise each aggregate's NULL handling.
#[test]
fn compliance_fixtures_execute_with_expected_answers() {
    let db = duckdb::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "ATTACH ':memory:' AS samples; CREATE SCHEMA samples.sales;
         CREATE TABLE samples.sales.orders (order_id INTEGER, customer_id INTEGER, amount DECIMAL(10, 2), status VARCHAR);
         INSERT INTO samples.sales.orders VALUES
           (1, 10, 100.00, 'completed'), (2, 10, 50.50, 'returned'), (3, 11, NULL, 'completed'),
           (4, 12, 25.25, 'completed'), (5, NULL, 10.00, 'open');",
    )
    .unwrap();
    let expected: &[(&str, &str, f64)] = &[
        ("m0_ansi_qualified", "total_amount", 185.75),
        ("m0_ossiesql_qualified", "total_amount", 185.75),
        ("m0_sum_bare", "total_amount", 185.75),
        ("m1_scalar_field", "total_amount", 185.75),
        ("m2_aggregates_qualified", "sum_amount", 185.75),
        ("m2_aggregates_qualified", "count_orders", 5.0),
        ("m2_aggregates_qualified", "distinct_customers", 3.0),
        ("m2_aggregates_qualified", "avg_amount", 46.4375), // NULL amount excluded: 185.75 / 4
        ("m2_aggregates_qualified", "min_amount", 10.0),
        ("m2_aggregates_qualified", "max_amount", 100.0),
        ("m2_aggregates_bare", "avg_amount", 46.4375),
        ("m3_filtered_agg_qualified", "completed_amount", 125.25), // NULL amount adds nothing
        ("m3_filtered_agg_bare", "completed_amount", 125.25),
    ];
    for (fixture, metric, want) in expected {
        let g = ossiec::from_yaml(&fixture_file(&format!("{fixture}.ossie.yaml"))).unwrap();
        let req = ossiec::Request {
            metrics: vec![metric.to_string().into()],
            ..Default::default()
        };
        let opts = ossiec::Options {
            target: Some(ossiec::DialectType::DuckDB),
            ..Default::default()
        };
        let sql = ossiec::compile(&g, &req, &opts).unwrap().sql;
        let got: f64 = db
            .query_row(&format!("SELECT CAST(({sql}) AS DOUBLE)"), [], |r| r.get(0))
            .unwrap_or_else(|e| panic!("{fixture}/{metric}: {e}\n{sql}"));
        assert!(
            (got - want).abs() < 1e-9,
            "{fixture}/{metric}: got {got}, want {want}\n{sql}"
        );
    }
    // The scalar field computes per row: amount * 1.1.
    let g = ossiec::from_yaml(&fixture_file("m1_scalar_field.ossie.yaml")).unwrap();
    let req: ossiec::Request = serde_json::from_str(r#"{"dimensions": ["sales.amount_with_tax"], "filters": [{"field": "sales.amount", "op": "=", "value": 100}]}"#).unwrap();
    let opts = ossiec::Options {
        target: Some(ossiec::DialectType::DuckDB),
        ..Default::default()
    };
    let sql = ossiec::compile(&g, &req, &opts).unwrap().sql;
    let got: f64 = db
        .query_row(&format!("SELECT CAST(({sql}) AS DOUBLE)"), [], |r| r.get(0))
        .unwrap();
    assert!(
        (got - 110.0).abs() < 1e-9,
        "amount_with_tax: got {got}\n{sql}"
    );
}
