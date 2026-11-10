//! Appendix A.2/A.3: derived tables joined into an `AGG` stage, outer queries
//! over them, nesting limits, and compiling them for every engine.

use super::*;

/// Appendix A.2 (`relational_semantics.md`): top-N self-join. Spec names the fact
/// `orders` and ranks on `total_revenue` while measuring `total_quantity`; this
/// fixture uses `sales` for the same shape.
const A2_APPENDIX: &str = "SELECT s.category, AGG(s.total_quantity) \
    FROM sales s \
    JOIN (SELECT state_id, AGG(total_revenue) AS r \
          FROM sales GROUP BY state_id \
          ORDER BY r DESC, state_id LIMIT 2) t \
      ON s.state_id = t.state_id \
    GROUP BY s.category";

/// Appendix A.3: average-of-averages over states (`sales` stands in for `orders`).
const A3_APPENDIX: &str = "SELECT d.category, AVG(m.state_revenue) \
    FROM (SELECT category, state_id FROM sales GROUP BY category, state_id) d \
    JOIN (SELECT state_id, AGG(total_revenue) AS state_revenue FROM sales GROUP BY state_id) m \
      ON d.state_id IS NOT DISTINCT FROM m.state_id \
    GROUP BY d.category";

fn agg_inner() -> String {
    "SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id".to_string()
}

fn shell_chain(levels: usize) -> String {
    let mut q = agg_inner();
    for i in 0..levels {
        q = format!("SELECT AVG(n) AS n FROM ({q}) a{i}");
    }
    q
}

fn many_derived(n: usize) -> String {
    let mut sql = format!("SELECT COUNT(*) AS c FROM ({}) m0", agg_inner());
    for i in 1..n {
        sql.push_str(&format!(
            " JOIN ({}) m{i} ON m0.customer_id = m{i}.customer_id",
            agg_inner()
        ));
    }
    sql
}

fn native_null_safe(t: DialectType) -> bool {
    matches!(
        t,
        DialectType::DuckDB
            | DialectType::PostgreSQL
            | DialectType::Trino
            | DialectType::Athena
            | DialectType::Databricks
            | DialectType::Spark
    )
}

#[test]
fn a2_shop_form_executes_with_top_n_semantics() {
    let c = ossiec::compile_layer2(&graph(), A2_SHOP, &duck()).unwrap();
    assert!(!c.sql.contains("__ossiec_deriv"), "{}", c.sql);
    assert!(c.sql.to_uppercase().contains("INNER JOIN"), "{}", c.sql);
    let db = duckdb();
    let got = sorted(duckdb_rows(&db, &c.sql));
    let want = sorted(duckdb_rows(&db, A2_SHOP_REF));
    assert_eq!(got, want);
    // Customers 2 (three orders) and 1 (two; tie with 3 broken by id) only:
    // 4 completed + 1 returned. Not the unrestricted 8 / 1.
    assert_eq!(got.len(), 2, "{got:?}");
    let total: f64 = got.iter().map(|r| r[1].parse::<f64>().unwrap()).sum();
    assert_eq!(total, 5.0, "{got:?}");
}

#[test]
fn a2_left_derived_join_does_not_filter() {
    let sql = A2_SHOP.replace("JOIN (SELECT", "LEFT JOIN (SELECT");
    let c = ossiec::compile_layer2(&graph(), &sql, &duck()).unwrap();
    assert!(c.sql.to_uppercase().contains("LEFT JOIN"), "{}", c.sql);
    let db = duckdb();
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(
            &db,
            "SELECT status, COUNT(*) FROM orders GROUP BY status"
        ))
    );
}

#[test]
fn a2_on_both_columns_is_unique_and_accepted() {
    let sql = "SELECT o.status, AGG(o.order_count) FROM orders o \
        JOIN (SELECT status, customer_id, AGG(order_count) AS n FROM orders GROUP BY status, customer_id) t \
          ON o.customer_id = t.customer_id AND o.status = t.status \
        GROUP BY o.status";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    let db = duckdb();
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(
            &db,
            "SELECT status, COUNT(*) FROM orders WHERE customer_id IS NOT NULL GROUP BY status"
        ))
    );
}

#[test]
fn a2_refuses_non_unique_derived_key_g2() {
    let sql = "SELECT o.status, AGG(o.order_count) FROM orders o \
        JOIN (SELECT status, customer_id, AGG(order_count) AS n FROM orders GROUP BY status, customer_id) t \
          ON o.customer_id = t.customer_id \
        GROUP BY o.status";
    let m = unsupported_msg(ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err());
    assert!(m.contains("G2") && m.contains("not unique"), "{m}");
}

#[test]
fn a2_unevaluated_measure_passes_through_inner_select() {
    // §7.7 in the A.2 join form: `order_count` stays a measure through the inner
    // select and the outer `AGG(t.order_count)` evaluates it.
    let sql = "SELECT o.status, AGG(t.order_count) \
        FROM orders o \
        JOIN (SELECT customer_id, order_count FROM orders) t \
          ON o.customer_id = t.customer_id \
        GROUP BY o.status";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    assert!(!c.sql.contains("__ossiec"), "{}", c.sql);
    let db = duckdb();
    let got = sorted(duckdb_rows(&db, &c.sql));
    // The derived table is only a restriction (customers who ordered); the measure
    // is evaluated by the outer stage, per status: guest checkouts drop out.
    let want = sorted(duckdb_rows(
        &db,
        "SELECT status, COUNT(*) FROM orders WHERE customer_id IS NOT NULL GROUP BY status",
    ));
    assert_eq!(got, want);
    assert_eq!(got.len(), 2, "{got:?}");
    // Passing the measure through without reading it changes nothing.
    let unused = sql.replace("AGG(t.order_count)", "AGG(o.order_count)");
    let c = ossiec::compile_layer2(&graph(), &unused, &duck()).unwrap();
    assert_eq!(sorted(duckdb_rows(&db, &c.sql)), want);
}

#[test]
fn layer2_nested_cases_match_their_references_on_duckdb() {
    // The engine tier's shop cases, checked here without Docker.
    let db = duckdb();
    let two_facts = ossiec::from_yaml(common::TWO_FACT_SHOP).unwrap();
    let cases = LAYER2_NESTED_CASES.iter().map(|c| (graph(), c)).chain(
        common::LAYER2_TWO_FACT_CASES
            .iter()
            .map(|c| (two_facts.clone(), c)),
    );
    for (g, case) in cases {
        let c = if case.script {
            ossiec::compile_layer2_script(&g, case.sql, &duck())
        } else {
            ossiec::compile_layer2(&g, case.sql, &duck())
        }
        .unwrap_or_else(|e| panic!("{}: {e}", case.name));
        assert_eq!(
            sorted(duckdb_rows(&db, &c.sql)),
            sorted(duckdb_rows(&db, case.reference)),
            "{}: {}",
            case.name,
            c.sql
        );
    }
}

#[test]
fn a2_refuses_inner_agg_without_alias() {
    let sql = "SELECT o.status, AGG(o.order_count) FROM orders o \
        JOIN (SELECT customer_id, AGG(order_count) FROM orders GROUP BY customer_id) t \
          ON o.customer_id = t.customer_id \
        GROUP BY o.status";
    let m = unsupported_msg(ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err());
    assert!(m.contains("explicit alias"), "{m}");
}

#[test]
fn a2_refuses_duplicate_inner_output_names() {
    let sql = "SELECT o.status, AGG(o.order_count) FROM orders o \
        JOIN (SELECT customer_id, AGG(order_count) AS customer_id FROM orders GROUP BY customer_id) t \
          ON o.customer_id = t.customer_id \
        GROUP BY o.status";
    let e = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("two output columns named")),
        "{e}"
    );
}

#[test]
fn a2_refuses_correlated_inner_query() {
    let sql = "SELECT o.status, AGG(o.order_count) FROM orders o \
        JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders \
              WHERE o.status = 'completed' GROUP BY customer_id) t \
          ON o.customer_id = t.customer_id \
        GROUP BY o.status";
    let m = unsupported_msg(ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err());
    assert!(m.contains("correlated"), "{m}");
}

#[test]
fn a2_refuses_derived_column_outside_on() {
    let sql = "SELECT t.n, AGG(o.order_count) FROM orders o \
        JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id) t \
          ON o.customer_id = t.customer_id \
        GROUP BY t.n";
    let m = unsupported_msg(ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err());
    assert!(m.contains("only appear in") && m.contains("ON"), "{m}");
}

#[test]
fn a2_refuses_set_operator_inside_derived_table() {
    {
        let inner = "SELECT customer_id FROM orders UNION SELECT customer_id FROM orders";
        let sql = format!(
            "SELECT o.status, AGG(o.order_count) FROM orders o JOIN ({inner}) t ON o.customer_id = t.customer_id GROUP BY o.status"
        );
        let e = ossiec::compile_layer2(&graph(), &sql, &duck()).unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{inner}: {e}");
    }
}

#[test]
fn inner_subquery_alone_matches_the_one_embedded_in_the_outer() {
    let g = graph();
    let compact = Options {
        pretty: false,
        ..duck()
    };
    let inner = ossiec::compile_layer2(
        &g,
        "SELECT customer_id AS customer_id, AGG(order_count) AS n FROM orders \
         GROUP BY customer_id ORDER BY n DESC, customer_id LIMIT 2",
        &compact,
    )
    .unwrap();
    let outer = ossiec::compile_layer2(&g, A2_SHOP, &compact).unwrap();
    assert!(
        outer.sql.contains(&inner.sql),
        "inner:\n{}\nouter:\n{}",
        inner.sql,
        outer.sql
    );
}

#[test]
fn a2_appendix_top_n_self_join() {
    let g = nested_graph();
    let c = ossiec::compile_layer2(&g, A2_APPENDIX, &duck()).unwrap();
    let db = nested_db();
    let got = sorted(duckdb_rows(&db, &c.sql));
    // Top two states by revenue: 3 (101) then 2 (35).
    let want = sorted(duckdb_rows(
        &db,
        "SELECT category, SUM(qty) FROM sales WHERE state_id IN (3, 2) GROUP BY category",
    ));
    assert_eq!(got, want);
    assert_eq!(got.len(), 2);
}

#[test]
fn a2_pk_dedup_outer_measure_keeps_the_derived_restriction() {
    // `total_population` lives on `states`, one side of the join: planned as PkDedup,
    // so the restriction must also appear in the de-duplicating sub-select.
    let g = nested_graph();
    let sql = "SELECT s.category, AGG(st.total_population) AS pop \
        FROM sales s \
        JOIN states st ON s.state_id = st.id \
        JOIN (SELECT state_id, AGG(total_revenue) AS r FROM sales GROUP BY state_id \
              ORDER BY r DESC, state_id LIMIT 2) t ON s.state_id = t.state_id \
        GROUP BY s.category";
    let c = ossiec::compile_layer2(&g, sql, &duck()).unwrap();
    assert!(c.sql.to_uppercase().contains("DISTINCT"), "{}", c.sql);
    let db = nested_db();
    let got = sorted(duckdb_rows(&db, &c.sql));
    let want = sorted(duckdb_rows(
        &db,
        "SELECT category, SUM(population) FROM (SELECT DISTINCT s.category, st.id, st.population \
           FROM sales s JOIN states st ON s.state_id = st.id WHERE st.id IN (3, 2)) GROUP BY category",
    ));
    assert_eq!(got, want);
    // A: states 2 and 3 (500); B: state 3 (300).
    assert_eq!(got.len(), 2, "{got:?}");
}

#[test]
fn a2_null_safe_derived_key_keeps_null_group() {
    let g = nested_graph();
    let q = |op: &str| {
        format!(
            "SELECT s.category, AGG(s.total_revenue) FROM sales s \
             JOIN (SELECT state_id, AGG(total_revenue) AS r FROM sales GROUP BY state_id \
                   ORDER BY r DESC, state_id LIMIT 4) t ON s.state_id {op} t.state_id \
             GROUP BY s.category"
        )
    };
    let db = nested_db();
    // Top four by revenue: states 3, 2, 1 and the NULL state (7).
    let c = ossiec::compile_layer2(&g, &q("IS NOT DISTINCT FROM"), &duck()).unwrap();
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(
            &db,
            "SELECT category, SUM(amount) FROM sales \
             WHERE state_id IN (1, 2, 3) OR state_id IS NULL GROUP BY category"
        ))
    );
    let c = ossiec::compile_layer2(&g, &q("="), &duck()).unwrap();
    assert_eq!(
        sorted(duckdb_rows(&db, &c.sql)),
        sorted(duckdb_rows(
            &db,
            "SELECT category, SUM(amount) FROM sales WHERE state_id IN (1, 2, 3) GROUP BY category"
        ))
    );
}

#[test]
fn a3_appendix_two_stage_aggregation() {
    let g = nested_graph();
    let c = ossiec::compile_layer2(&g, A3_APPENDIX, &duck()).unwrap();
    let db = nested_db();
    let got = sorted(duckdb_rows(&db, &c.sql));
    // A: states 1, 2, 3 and NULL → (30 + 35 + 101 + 7) / 4; B: 1, 3, 4 → (30 + 101 + 2) / 3.
    let want = sorted(duckdb_rows(
        &db,
        "SELECT category, AVG(r) FROM (SELECT DISTINCT d.category, d.state_id, m.r FROM \
           (SELECT DISTINCT category, state_id FROM sales) d JOIN \
           (SELECT state_id, SUM(amount) AS r FROM sales GROUP BY state_id) m \
           ON d.state_id IS NOT DISTINCT FROM m.state_id) GROUP BY category",
    ));
    assert_eq!(got, want);
    let a = got.iter().find(|r| r[0] == "A").unwrap();
    assert_eq!(a[1].parse::<f64>().unwrap(), 43.25, "{got:?}");
}

#[test]
fn a3_plain_equality_drops_null_keys() {
    let g = nested_graph();
    let c = ossiec::compile_layer2(
        &g,
        &A3_APPENDIX.replace("IS NOT DISTINCT FROM", "="),
        &duck(),
    )
    .unwrap();
    let db = nested_db();
    let got = sorted(duckdb_rows(&db, &c.sql));
    let a = got.iter().find(|r| r[0] == "A").unwrap();
    assert!(
        (a[1].parse::<f64>().unwrap() - 166.0 / 3.0).abs() < 1e-5,
        "{got:?}"
    );
}

#[test]
fn a3_shop_form_executes() {
    let c = ossiec::compile_layer2(&graph(), A3_SHOP, &duck()).unwrap();
    assert!(c.sql.contains("IS NOT DISTINCT FROM"), "{}", c.sql);
    assert!(!c.sql.contains("__ossiec_deriv"), "{}", c.sql);
    let db = duckdb();
    let got = sorted(duckdb_rows(&db, &c.sql));
    assert_eq!(got, sorted(duckdb_rows(&db, A3_SHOP_REF)));
    assert_eq!(got.len(), 2, "{got:?}");
}

#[test]
fn a3_shell_where_having_order_limit_and_aliases() {
    let sql = "SELECT d.status, COUNT(*) AS k, MAX(m.n) AS biggest, SUM(m.n) \
        FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
        LEFT JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id) m \
          ON d.customer_id = m.customer_id \
        WHERE m.n >= 1 \
        GROUP BY d.status \
        HAVING COUNT(*) >= 1 \
        ORDER BY k DESC, status \
        LIMIT 5";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    let upper = c.sql.to_uppercase();
    assert!(
        upper.contains("HAVING") && upper.contains("LIMIT 5"),
        "{}",
        c.sql
    );
    assert!(c.sql.contains("\"sum_n\""), "default name: {}", c.sql);
    let db = duckdb();
    let want = duckdb_rows(
        &db,
        "SELECT d.status, COUNT(*), MAX(m.n), SUM(m.n) \
         FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
         LEFT JOIN (SELECT customer_id, COUNT(*) AS n FROM orders GROUP BY customer_id) m \
           ON d.customer_id = m.customer_id \
         WHERE m.n >= 1 GROUP BY d.status HAVING COUNT(*) >= 1 ORDER BY 2 DESC, 1 LIMIT 5",
    );
    assert_eq!(duckdb_rows(&db, &c.sql), want);
}

#[test]
fn a3_shell_without_group_by_is_one_row() {
    let sql = "SELECT AVG(m.n), COUNT(*) FROM \
        (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id) m";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    let db = duckdb();
    assert_eq!(
        duckdb_rows(&db, &c.sql),
        duckdb_rows(
            &db,
            "SELECT AVG(n), COUNT(*) FROM (SELECT COUNT(*) AS n FROM orders GROUP BY customer_id)"
        )
    );
}

#[test]
fn a3_shells_nest() {
    let sql = "SELECT AVG(a.top) AS avg_top FROM ( \
        SELECT d.status, MAX(m.n) AS top \
        FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
        JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id) m \
          ON d.customer_id = m.customer_id \
        GROUP BY d.status) a";
    let c = ossiec::compile_layer2(&graph(), sql, &duck()).unwrap();
    let db = duckdb();
    assert_eq!(
        duckdb_rows(&db, &c.sql),
        duckdb_rows(
            &db,
            "SELECT AVG(top) FROM (SELECT d.status, MAX(m.n) AS top \
             FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
             JOIN (SELECT customer_id, COUNT(*) AS n FROM orders GROUP BY customer_id) m \
               ON d.customer_id = m.customer_id GROUP BY d.status)"
        )
    );
}

#[test]
fn a3_refuses_agg_over_an_evaluated_column() {
    let sql = "SELECT d.status, AGG(m.n) \
        FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
        JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id) m \
          ON d.customer_id = m.customer_id GROUP BY d.status";
    let m = unsupported_msg(ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err());
    assert!(m.contains("already evaluated"), "{m}");
}

#[test]
fn a3_refuses_raw_dataset_next_to_derived_tables() {
    let sql = "SELECT d.status, COUNT(*) \
        FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
        JOIN orders o ON d.customer_id = o.customer_id GROUP BY d.status";
    let m = unsupported_msg(ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err());
    assert!(m.contains("raw model tables"), "{m}");
}

#[test]
fn a3_refuses_group_by_mismatch_and_unknown_columns() {
    let base = "FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d";
    let e = ossiec::compile_layer2(
        &graph(),
        &format!("SELECT d.status, COUNT(*) {base} GROUP BY d.customer_id"),
        &duck(),
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("GROUP BY must match")),
        "{e}"
    );
    let e = ossiec::compile_layer2(
        &graph(),
        &format!("SELECT d.nope, COUNT(*) {base} GROUP BY d.nope"),
        &duck(),
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Request(ref m) if m.contains("no column `nope`")),
        "{e}"
    );
}

#[test]
fn a3_refuses_correlated_reference_in_shell_subquery() {
    let sql = "SELECT d.status, COUNT(*) \
        FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d \
        JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders WHERE d.status = 'x' GROUP BY customer_id) m \
          ON d.customer_id = m.customer_id GROUP BY d.status";
    let m = unsupported_msg(ossiec::compile_layer2(&graph(), sql, &duck()).unwrap_err());
    assert!(m.contains("correlated"), "{m}");
}

#[test]
fn nesting_depth_is_limited_to_four() {
    let g = graph();
    ossiec::compile_layer2(&g, &shell_chain(4), &duck()).unwrap();
    let m = unsupported_msg(ossiec::compile_layer2(&g, &shell_chain(5), &duck()).unwrap_err());
    assert!(m.contains("at most 4 levels"), "{m}");
}

#[test]
fn derived_tables_per_query_are_limited_to_eight() {
    let g = graph();
    ossiec::compile_layer2(&g, &many_derived(8), &duck()).unwrap();
    let m = unsupported_msg(ossiec::compile_layer2(&g, &many_derived(9), &duck()).unwrap_err());
    assert!(m.contains("at most 8 derived tables"), "{m}");
}

#[test]
fn nested_queries_compile_on_every_engine() {
    let g = graph();
    for target in ENGINES {
        let opts = Options {
            target: Some(target),
            ..Default::default()
        };
        for (name, q) in [("A.2", A2_SHOP), ("A.3", A3_SHOP)] {
            let c = ossiec::compile_layer2(&g, q, &opts)
                .unwrap_or_else(|e| panic!("{name} on {target:?}: {e}"));
            assert!(!c.sql.contains("__ossiec"), "{name} {target:?}: {}", c.sql);
            polyglot_sql::parse_one(&c.sql, target)
                .unwrap_or_else(|e| panic!("{name} {target:?} does not parse: {e}\n{}", c.sql));
            if name == "A.3" {
                let native = c.sql.to_uppercase().contains("IS NOT DISTINCT FROM");
                assert_eq!(
                    native,
                    native_null_safe(target),
                    "null-safe join on {target:?}:\n{}",
                    c.sql
                );
                if !native {
                    assert!(
                        c.sql.to_uppercase().contains("IS NULL"),
                        "{target:?}: {}",
                        c.sql
                    );
                    assert!(!c.sql.contains("<=>"), "{target:?}: {}", c.sql);
                }
            }
            if target == DialectType::ClickHouse {
                assert_eq!(c.sql.matches("join_use_nulls").count(), 1, "{}", c.sql);
            }
        }
    }
}

/// Phase 1a spike: lower the outer stage with a placeholder relation, adapt each
/// stage with its own plan, substitute, render once. The derived alias and its
/// quoting must survive on every engine family we adapt for.
#[test]
fn spike_placeholder_substitution_keeps_alias_and_quoting() {
    let g = graph();
    for (target, quoted_t) in [
        (DialectType::DuckDB, "\"t\""),
        (DialectType::PostgreSQL, "\"t\""),
        (DialectType::MySQL, "`t`"),
        (DialectType::ClickHouse, "\"t\""),
    ] {
        let c = ossiec::compile_layer2(
            &g,
            A2_SHOP,
            &Options {
                target: Some(target),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!c.sql.contains("__ossiec_deriv"), "{target:?}: {}", c.sql);
        assert!(
            c.sql.contains(&format!("AS {quoted_t}")) && c.sql.contains(&format!("{quoted_t}.")),
            "{target:?} lost the derived alias: {}",
            c.sql
        );
        polyglot_sql::parse_one(&c.sql, target)
            .unwrap_or_else(|e| panic!("{target:?} does not parse: {e}\n{}", c.sql));
    }
    // And it runs.
    let c = ossiec::compile_layer2(&g, A2_SHOP, &duck()).unwrap();
    assert!(!duckdb_rows(&duckdb(), &c.sql).is_empty());
}

#[test]
fn each_stage_is_adapted_with_its_own_plan() {
    let g = graph();
    let pg = ossiec::compile_layer2(
        &g,
        A3_SHOP,
        &Options {
            target: Some(DialectType::PostgreSQL),
            ..Default::default()
        },
    )
    .unwrap();
    // The `d` stage's string dimension takes Postgres's byte collation (its plan's
    // `string_dims`), under the consumer-visible name `status`.
    assert!(pg.sql.contains("COLLATE \"C\""), "{}", pg.sql);
    let my = ossiec::compile_layer2(
        &g,
        A3_SHOP,
        &Options {
            target: Some(DialectType::MySQL),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(my.sql.contains("COLLATE utf8mb4_bin"), "{}", my.sql);
    assert!(
        my.sql.to_uppercase().contains("CAST("),
        "outer AVG: {}",
        my.sql
    );
}

#[test]
fn derived_join_attach_dataset_is_planned_for_grand_total() {
    // Measure is on orders; attach is customers. Ensure customers is in the plan.
    let sql = "SELECT AGG(m.x) FROM customers c
        JOIN (SELECT customer_id, order_amt AS x FROM orders) m
          ON c.id = m.customer_id";
    let c = ossiec::compile_layer2(&two_facts_graph(), sql, &two_facts_opts()).unwrap();
    let got = duckdb_rows(&two_facts_db(), &c.sql);
    assert_eq!(got.len(), 1, "{got:?}");
    // 10+20+30+5 = 65
    assert_eq!(got[0][0].parse::<f64>().unwrap(), 65.0);
}
