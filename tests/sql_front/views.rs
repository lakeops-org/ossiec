//! Appendix A.7: `CREATE VIEW` measure relations in a session catalog.

use super::*;

#[test]
fn create_view_agg_of_measure_matches_inline_ratio() {
    let (g, catalog) = define_region_metrics();
    let via = "SELECT region, AGG(sales_per_ticket) AS spt \
        FROM region_metrics GROUP BY region";
    let inline = format!(
        "SELECT region, AGG(m.sales_per_ticket) AS spt \
         FROM ({SALES_PER_TICKET}) m GROUP BY region"
    );
    let c = ossiec::compile_layer2_with(&g, &catalog, via, &two_facts_opts()).unwrap();
    let direct = ossiec::compile_layer2(&g, &inline, &two_facts_opts()).unwrap();
    assert_eq!(c.sql, direct.sql);
    assert!(
        !c.sql.contains("__l2_tm") && !c.sql.contains("__ossiec"),
        "{}",
        c.sql
    );
    assert!(c.sql.contains("UNION ALL"), "{}", c.sql);
    assert_region_ratio(&c.sql, true);
    // The catalog is session state: compile_layer2 does not see the view.
    let missing = ossiec::compile_layer2(&g, via, &two_facts_opts()).unwrap_err();
    assert!(
        matches!(missing, Error::Request(ref m) if m.contains("region_metrics")),
        "{missing}"
    );
}

#[test]
fn create_view_join_form_matches_derived_table() {
    let (g, catalog) = define_region_metrics();
    let via = "SELECT r.region, AGG(m.sales_per_ticket) AS spt \
        FROM customers r \
        JOIN region_metrics m ON r.region = m.region \
        GROUP BY r.region";
    let inline = format!(
        "SELECT r.region, AGG(m.sales_per_ticket) AS spt \
         FROM customers r JOIN ({SALES_PER_TICKET}) m ON r.region = m.region \
         GROUP BY r.region"
    );
    let c = ossiec::compile_layer2_with(&g, &catalog, via, &two_facts_opts()).unwrap();
    let direct = ossiec::compile_layer2(&g, &inline, &two_facts_opts()).unwrap();
    assert_eq!(c.sql, direct.sql);
    assert_region_ratio(&c.sql, false);
}

#[test]
fn create_view_outer_filter_keeps_one_region() {
    let (g, catalog) = define_region_metrics();
    let c = ossiec::compile_layer2_with(
        &g,
        &catalog,
        "SELECT region, AGG(sales_per_ticket) AS spt FROM region_metrics \
         WHERE region = 'EU' GROUP BY region",
        &two_facts_opts(),
    )
    .unwrap();
    let got = rows_f64(duckdb_rows(&two_facts_db(), &c.sql));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, "EU");
    assert!((got[0].1 - 20.0).abs() < 1e-9, "{got:?}");
}

#[test]
fn create_view_dimension_only_and_pass_through() {
    let g = two_facts_graph();
    let mut catalog = Layer2Catalog::new();
    ossiec::define_layer2_view(
        &g,
        &mut catalog,
        "CREATE VIEW eu_ids AS SELECT c.id FROM customers c WHERE c.region = 'EU'",
        &two_facts_opts(),
    )
    .unwrap();
    let cols = catalog.columns("eu_ids").unwrap();
    assert_eq!(cols.len(), 1);
    assert_eq!(cols[0].kind, Layer2ColumnKind::Field);
    let listed =
        ossiec::compile_layer2_with(&g, &catalog, "SELECT id FROM eu_ids", &two_facts_opts())
            .unwrap();
    assert_eq!(
        sorted(duckdb_rows(&two_facts_db(), &listed.sql)),
        sorted(duckdb_rows(
            &two_facts_db(),
            "SELECT id FROM customers WHERE region = 'EU'"
        ))
    );
    // A second view reads the first. AGG of the passed-through measure is the EU total.
    ossiec::define_layer2_view(
        &g,
        &mut catalog,
        "CREATE VIEW eu_orders AS \
         SELECT o.customer_id, o.order_amt FROM orders o \
         JOIN eu_ids e ON o.customer_id = e.id",
        &two_facts_opts(),
    )
    .unwrap();
    let summed = ossiec::compile_layer2_with(
        &g,
        &catalog,
        "SELECT AGG(order_amt) AS amt FROM eu_orders",
        &two_facts_opts(),
    )
    .unwrap();
    assert_eq!(
        duckdb_rows(&two_facts_db(), &summed.sql),
        duckdb_rows(
            &two_facts_db(),
            "SELECT SUM(amount) FROM orders WHERE customer_id IN (1, 2)"
        )
    );
}

#[test]
fn create_view_script_splits_on_semicolons_outside_strings() {
    let g = two_facts_graph();
    let script = format!(
        "{REGION_METRICS};\n\
         -- a comment;\n\
         SELECT region, AGG(sales_per_ticket) AS spt FROM region_metrics GROUP BY region;"
    );
    let c = ossiec::compile_layer2_script(&g, &script, &two_facts_opts()).unwrap();
    assert_region_ratio(&c.sql, true);
    // A semicolon inside a string is not a statement boundary.
    let mut catalog = Layer2Catalog::new();
    ossiec::define_layer2_view(
        &g,
        &mut catalog,
        "CREATE VIEW labeled AS SELECT c.region FROM customers c WHERE c.region = 'a;b'",
        &two_facts_opts(),
    )
    .unwrap();
    let c = ossiec::compile_layer2_with(
        &g,
        &catalog,
        "SELECT region FROM labeled",
        &two_facts_opts(),
    )
    .unwrap();
    assert!(duckdb_rows(&two_facts_db(), &c.sql).is_empty());
}

#[test]
fn create_view_refusals() {
    let g = two_facts_graph();
    let opts = two_facts_opts();
    let body = "SELECT c.region FROM customers c";
    for sql in [
        "CREATE OR REPLACE VIEW region_metrics AS SELECT c.region FROM customers c",
        "CREATE OR ALTER VIEW region_metrics AS SELECT c.region FROM customers c",
        "CREATE MATERIALIZED VIEW region_metrics AS SELECT c.region FROM customers c",
        "CREATE VIEW region_metrics (region) AS SELECT c.region FROM customers c",
    ] {
        let e = ossiec::define_layer2_view(&g, &mut Layer2Catalog::new(), sql, &opts).unwrap_err();
        assert!(
            matches!(e, Error::Unsupported(ref m) if m.contains("CREATE VIEW") || m.contains("OR REPLACE") || m.contains("OR ALTER")),
            "{sql}: {e}"
        );
    }
    let e = ossiec::define_layer2_view(
        &g,
        &mut Layer2Catalog::new(),
        "DROP VIEW region_metrics",
        &opts,
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("DROP VIEW")),
        "{e}"
    );
    let e = ossiec::compile_layer2(&g, "DROP VIEW region_metrics", &opts).unwrap_err();
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("DROP VIEW")),
        "{e}"
    );
    let e = ossiec::compile_layer2(&g, REGION_METRICS, &opts).unwrap_err();
    assert!(
        matches!(e, Error::Unsupported(ref m) if m.contains("define_layer2_view")),
        "{e}"
    );
    let e = ossiec::define_layer2_view(
        &g,
        &mut Layer2Catalog::new(),
        &format!("CREATE VIEW customers AS {body}"),
        &opts,
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("collides") && m.contains("customers")),
        "{e}"
    );
    let mut catalog = Layer2Catalog::new();
    ossiec::define_layer2_view(&g, &mut catalog, REGION_METRICS, &opts).unwrap();
    let e = ossiec::define_layer2_view(&g, &mut catalog, REGION_METRICS, &opts).unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("already defined")),
        "{e}"
    );
    let e = ossiec::define_layer2_view(
        &g,
        &mut Layer2Catalog::new(),
        "CREATE VIEW self_view AS SELECT c.region FROM self_view c",
        &opts,
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("refers to itself")),
        "{e}"
    );
    let e = ossiec::compile_layer2_script(&g, REGION_METRICS, &opts).unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("no SELECT")),
        "{e}"
    );
}

#[test]
fn create_view_compiles_on_every_engine() {
    let g = two_facts_graph();
    let query = "SELECT region, AGG(sales_per_ticket) AS spt FROM region_metrics GROUP BY region";
    for target in ENGINES {
        let opts = Options {
            target: Some(target),
            ..Default::default()
        };
        let mut catalog = Layer2Catalog::new();
        ossiec::define_layer2_view(&g, &mut catalog, REGION_METRICS, &opts)
            .unwrap_or_else(|e| panic!("define on {target:?}: {e}"));
        let c = ossiec::compile_layer2_with(&g, &catalog, query, &opts)
            .unwrap_or_else(|e| panic!("compile on {target:?}: {e}"));
        assert!(
            !c.sql.contains("__ossiec") && !c.sql.contains("__l2_tm"),
            "{target:?}: {}",
            c.sql
        );
        polyglot_sql::parse_one(&c.sql, target)
            .unwrap_or_else(|e| panic!("{target:?} does not parse: {e}\n{}", c.sql));
    }
}

#[test]
fn create_view_with_base_sum_to_measure() {
    // A.7 body with a base §5.1 measure, not only an AGG composition.
    let g = two_facts_graph();
    let mut catalog = Layer2Catalog::new();
    ossiec::define_layer2_view(
        &g,
        &mut catalog,
        "CREATE VIEW region_revenue AS \
         SELECT c.region, SUM(o.amount) TO MEASURE AS total_revenue \
         FROM customers c LEFT JOIN orders o ON o.customer_id = c.id",
        &two_facts_opts(),
    )
    .unwrap();
    let via = "SELECT region, AGG(total_revenue) AS total_revenue \
        FROM region_revenue GROUP BY region";
    let fold = "SELECT region, AGG(total_revenue) AS total_revenue FROM ( \
         SELECT c.region, SUM(o.amount) TO MEASURE AS total_revenue \
         FROM customers c LEFT JOIN orders o ON o.customer_id = c.id) m GROUP BY region";
    let c = ossiec::compile_layer2_with(&g, &catalog, via, &two_facts_opts()).unwrap();
    let direct = ossiec::compile_layer2(&g, fold, &two_facts_opts()).unwrap();
    assert_eq!(
        c.sql, direct.sql,
        "view expands to the same fold as an inline derived FROM"
    );
    let got = rows_f64(duckdb_rows(&two_facts_db(), &c.sql));
    let want = rows_f64(duckdb_rows(
        &two_facts_db(),
        "SELECT c.region, SUM(o.amount) \
         FROM customers c LEFT JOIN orders o ON o.customer_id = c.id \
         GROUP BY c.region",
    ));
    assert_eq!(got.len(), want.len(), "{got:?} vs {want:?}");
    for (a, b) in got.iter().zip(&want) {
        assert_eq!(a.0, b.0);
        assert!((a.1 - b.1).abs() < 1e-9, "{got:?} vs {want:?}");
    }
}

/// Appendix A.7: conformed-dimension bridge as a session `CREATE VIEW`.
#[test]
fn a7_appendix_conformed_dimension_bridge() {
    let (g, catalog) = define_region_metrics();
    let via = "SELECT region, AGG(sales_per_ticket) AS sales_per_ticket \
        FROM region_metrics GROUP BY region";
    let c = ossiec::compile_layer2_with(&g, &catalog, via, &two_facts_opts()).unwrap();
    assert_region_ratio(&c.sql, true);
}

/// A view can't define a measure and also aggregate with HAVING (§7.2), so there's
/// no such view to persist.
#[test]
fn a_view_defining_a_measure_cannot_have_having() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let mut catalog = Layer2Catalog::new();
    for view in [
        "CREATE VIEW v AS SELECT status, COUNT(*) TO MEASURE AS n FROM orders GROUP BY status HAVING AGG(order_count) > 1",
        "CREATE VIEW v AS SELECT status, COUNT(*) TO MEASURE AS n FROM orders HAVING AGG(order_count) > 1",
    ] {
        let e = ossiec::define_layer2_view(&g, &mut catalog, view, &duck())
            .expect_err(view)
            .to_string();
        assert!(e.contains("§7.2") || e.contains("HAVING"), "{view}: {e}");
    }
}

/// Appendix A.7, as the spec writes it: the view has no `GROUP BY`, and gives the
/// answer of the same measure in a plain query. Regions only: the NULL region also
/// holds orders without a customer, which `left_join_from_the_one_side...` covers.
#[test]
fn appendix_a7_ungrouped_view_answers() {
    let g = ossiec::from_yaml(common::MODEL).unwrap();
    let db = duckdb();
    let q = "CREATE VIEW v AS SELECT c.region, (AGG(o.order_count) * 2) TO MEASURE AS x \
             FROM customers c LEFT JOIN orders o ON o.customer_id = c.id; \
             SELECT region, AGG(x) FROM v WHERE region IS NOT NULL GROUP BY region";
    let c = ossiec::compile_sql_script(&g, q, &duck()).unwrap();
    let reference = "SELECT c.region, 2 * COUNT(o.order_id) FROM {customers} c \
                     LEFT JOIN orders o ON o.customer_id = c.customer_id \
                     WHERE c.region IS NOT NULL GROUP BY c.region";
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(&db, reference))
    );
}

/// §3.1: an unaliased `AGG(m)` is named by its final part, `m`, also when `m` is
/// a view's ratio, read from the view or joined in. It used to come back `__l2_1`.
#[test]
fn unaliased_agg_of_a_view_measure_takes_its_name() {
    let (g, catalog) = define_region_metrics();
    for q in [
        "SELECT region, AGG(sales_per_ticket) FROM region_metrics GROUP BY region",
        "SELECT r.region, AGG(m.sales_per_ticket) FROM customers r \
         JOIN region_metrics m ON r.region = m.region GROUP BY r.region",
    ] {
        let c = ossiec::compile_layer2_with(&g, &catalog, q, &two_facts_opts()).unwrap();
        assert!(c.sql.contains("AS \"sales_per_ticket\""), "{q}\n{}", c.sql);
        assert!(!c.sql.contains("__l2_"), "{q}\n{}", c.sql);
    }
    // A derived column renamed with AS is named by that name.
    let c = ossiec::compile_layer2(
        &graph(),
        "SELECT t.status, AGG(t.n) FROM (SELECT status, order_count AS n FROM orders) t GROUP BY t.status",
        &duck(),
    )
    .unwrap();
    assert!(c.sql.contains("AS \"n\""), "{}", c.sql);
}
