//! Loading and binding: every vendored Ossie example, and one test per validation rule.

use ossiec::Error;

fn fixture(name: &str) -> String {
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
        ossiec::from_yaml(&fixture(f)).unwrap_or_else(|e| panic!("{f}: {e}"));
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
        let g = ossiec::from_yaml(&fixture(&format!("{f}.ossie.yaml"))).unwrap();
        for m in g.describe().metrics {
            let req = ossiec::Request {
                metrics: vec![m.name.clone()],
                ..Default::default()
            };
            ossiec::compile(&g, &req, &Default::default())
                .unwrap_or_else(|e| panic!("{f}/{}: {e}", m.name));
        }
    }
}

#[test]
fn tpcds_window_metrics_compile_with_their_dimensions() {
    let g = ossiec::from_yaml(&fixture("tpcds_semantic_model.yaml")).unwrap();
    for m in g.describe().metrics {
        assert!(m.queryable, "{}", m.name);
        let req = ossiec::Request {
            metrics: vec![m.name.clone()],
            dimensions: m.requires_dimensions.clone(),
            ..Default::default()
        };
        ossiec::compile(&g, &req, &Default::default())
            .unwrap_or_else(|e| panic!("{}: {e}", m.name));
    }
}

#[test]
fn ontology_and_legacy_documents_are_rejected_clearly() {
    let e = ossiec::load(&fixture("flights.yaml"))
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
            "datasets:\n  - {name: a, source: t, fields: [{name: x, expression: {sql: x}}]}\nmetrics:\n  - {name: m, expression: {sql: a.x + 1}}\n",
            "a metric must aggregate",
        ),
        (
            "datasets:\n  - {name: a, source: t, fields: [{name: x, expression: {sql: x}}]}\nmetrics:\n  - {name: m, expression: {sql: SUM(a.x) + a.x}}\n",
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
        (
            "datasets:\n  - {name: a, source: t, fields: [{name: x, expression: {sql: x}}]}\nmetrics:\n  - {name: m, expression: {sql: COUNT(*)}}\n",
            "must be a dataset-scoped metric",
        ),
    ];
    for (body, want) in cases {
        let got = invalid(&format!("{HEAD}{body}"));
        assert!(got.contains(want), "want `{want}` in:\n{got}");
    }
}

#[test]
fn bare_identifiers_match_case_insensitively_quoted_ones_exactly() {
    let yaml = format!(
        "{HEAD}datasets:\n  - {{name: Orders, source: t, fields: [{{name: Amount, expression: {{sql: amt}}}}]}}\nmetrics:\n  - {{name: m, expression: {{sql: SUM(orders.AMOUNT)}}}}\n  - {{name: q, expression: {{sql: 'SUM(\"Orders\".\"Amount\")'}}}}\n"
    );
    ossiec::from_yaml(&yaml).unwrap();
    let bad = yaml.replace("\"Amount\")", "\"amount\")");
    assert_ne!(bad, yaml);
    assert!(invalid(&bad).contains("not a field"));
}
