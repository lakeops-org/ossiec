//! Binding a model: validation, dataset sources, field variants per dialect, and
//! the indexes the planner reads.

use std::cell::RefCell;
use std::collections::BTreeSet;

use polyglot_sql::expressions::Identifier;
use polyglot_sql::{contains_aggregate, contains_window_function, DialectType, Expression};

use crate::model::Model;
use crate::sql::{self, transform};
use crate::{Error, Result};

use super::{
    lookup, variants, Ctx, DatasetInfo, FieldInfo, Flavor, MetricInfo, RelInfo, SemanticGraph,
    Variant, MAX_IDENTIFIER, SOURCE_COLUMN,
};

/// Validate `model` and bind it into a [`SemanticGraph`].
///
/// Every problem is reported at once. Metrics that are valid Ossie but use
/// constructs this version can't compile yet (window functions) bind fine and
/// are marked as not queryable.
pub fn bind(model: Model) -> Result<SemanticGraph> {
    let mut errs = vec![];
    let mut datasets = vec![];

    dup_names(
        model.datasets.iter().map(|d| d.name.as_str()),
        "dataset",
        &mut errs,
    );
    check_length(&model.name, "model", &mut errs);
    for d in &model.datasets {
        check_length(&d.name, "dataset", &mut errs);
        for n in d.fields.iter().map(|f| &f.name) {
            check_length(n, "field", &mut errs);
        }
        for n in d.metrics.iter().map(|m| &m.name) {
            check_length(n, "metric", &mut errs);
        }
        dup_names(
            d.fields
                .iter()
                .map(|f| f.name.as_str())
                .chain(d.metrics.iter().map(|m| m.name.as_str())),
            &format!("field or metric in dataset `{}`", d.name),
            &mut errs,
        );
        let source = source_expr(&d.source, &d.name).unwrap_or_else(|e| {
            errs.push(format!("dataset `{}`: source {e}", d.name));
            sql::col("invalid_source")
        });
        let mut fields = vec![];
        for f in &d.fields {
            let what = format!("field `{}.{}`", d.name, f.name);
            let vs = match variants(&f.expression, &what) {
                Ok(vs) => vs,
                Err(e) => {
                    errs.push(e);
                    continue;
                }
            };
            for v in &vs {
                if contains_aggregate(&v.ast) || contains_window_function(&v.ast) {
                    errs.push(format!("{what}: field expressions are row-level; aggregates and window functions belong in metrics"));
                }
            }
            let is_time = f
                .dimension
                .as_ref()
                .and_then(|d| d.is_time)
                .unwrap_or_else(|| temporal(f.datatype.as_deref()));
            fields.push(FieldInfo {
                name: f.name.clone(),
                variants: vs,
                datatype: f.datatype.clone(),
                is_time,
            });
        }
        let key_of = |name: &str, what: &str, errs: &mut Vec<String>| match lookup(
            fields.iter().map(|f| f.name.as_str()),
            name,
            false,
        ) {
            Ok(Some(i)) => Some(i),
            Ok(None) => {
                errs.push(format!(
                    "dataset `{}`: {what} `{name}` is not a field",
                    d.name
                ));
                None
            }
            Err(e) => {
                errs.push(format!("dataset `{}`: {e}", d.name));
                None
            }
        };
        let primary_key = d
            .primary_key
            .iter()
            .filter_map(|k| key_of(k, "primary_key column", &mut errs))
            .collect();
        for uk in &d.unique_keys {
            for k in uk {
                key_of(k, "unique_keys column", &mut errs);
            }
        }
        datasets.push(DatasetInfo {
            name: d.name.clone(),
            source,
            fields,
            primary_key,
        });
    }

    let ds_index = |name: &str| {
        lookup(datasets.iter().map(|d| d.name.as_str()), name, false)
            .ok()
            .flatten()
    };
    let mut rels = vec![];
    dup_names(
        model.relationships.iter().map(|r| r.name.as_str()),
        "relationship",
        &mut errs,
    );
    for r in &model.relationships {
        check_length(&r.name, "relationship", &mut errs);
        let what = format!("relationship `{}`", r.name);
        let (Some(from), Some(to)) = (ds_index(&r.from), ds_index(&r.to)) else {
            errs.push(format!(
                "{what}: `from` and `to` must name datasets (got `{}` → `{}`)",
                r.from, r.to
            ));
            continue;
        };
        if r.from_columns.is_empty() || r.from_columns.len() != r.to_columns.len() {
            errs.push(format!(
                "{what}: from_columns and to_columns must be non-empty and the same length"
            ));
            continue;
        }
        let cols = |ds: usize, names: &[String], errs: &mut Vec<String>| -> Option<Vec<usize>> {
            names
                .iter()
                .map(|n| {
                    let hit = lookup(
                        datasets[ds].fields.iter().map(|f| f.name.as_str()),
                        n,
                        false,
                    )
                    .ok()
                    .flatten();
                    if hit.is_none() {
                        errs.push(format!(
                            "{what}: `{n}` is not a field of `{}`",
                            datasets[ds].name
                        ));
                    }
                    hit
                })
                .collect()
        };
        let (Some(from_fields), Some(to_fields)) = (
            cols(from, &r.from_columns, &mut errs),
            cols(to, &r.to_columns, &mut errs),
        ) else {
            continue;
        };
        rels.push(RelInfo {
            name: r.name.clone(),
            from,
            to,
            from_fields,
            to_fields,
        });
    }

    let mut metrics = vec![];
    dup_names(
        model.metrics.iter().map(|m| m.name.as_str()),
        "model metric",
        &mut errs,
    );
    let scoped = model
        .datasets
        .iter()
        .enumerate()
        .flat_map(|(i, d)| d.metrics.iter().map(move |m| (Some(i), m)));
    for (owner, m) in scoped.chain(model.metrics.iter().map(|m| (None, m))) {
        let name = match owner {
            Some(i) => format!("{}.{}", model.datasets[i].name, m.name),
            None => m.name.clone(),
        };
        if owner.is_none() {
            check_length(&m.name, "metric", &mut errs);
        }
        match variants(&m.expression, &format!("metric `{name}`")) {
            Ok(mut vs) => {
                if let Some(o) = owner {
                    let d = RefCell::new(&mut datasets[o]);
                    for v in &mut vs {
                        let ast = std::mem::replace(&mut v.ast, sql::col("x"));
                        v.ast = source_columns(ast, &d);
                    }
                }
                metrics.push(MetricInfo {
                    name,
                    owner,
                    variants: vs,
                    unsupported: None,
                    raw: false,
                })
            }
            Err(e) => errs.push(e),
        }
    }

    let mut g = SemanticGraph {
        model,
        datasets,
        rels,
        metrics,
        warnings: vec![],
    };
    let mut ctx = Ctx::default();
    for i in 0..g.metrics.len() {
        // Wide table §5.2: these are for a query that names the metric to refuse,
        // so they don't stop the rest of the model from loading.
        let rejections = match g.metric_rejections(i, &mut ctx) {
            Ok(r) => r,
            Err(Error::InvalidModel(es)) => {
                errs.extend(es);
                continue;
            }
            Err(other) => {
                errs.push(other.to_string());
                continue;
            }
        };
        match g.resolve_metric_unchecked(i, &mut ctx) {
            Ok(_) => {}
            Err(Error::InvalidModel(es)) if !rejections.is_empty() => {
                g.metrics[i].unsupported = Some(es.join("; "))
            }
            Err(Error::Unsupported(why)) => g.metrics[i].unsupported = Some(why),
            Err(Error::InvalidModel(es)) => errs.extend(es),
            Err(other) => errs.push(other.to_string()),
        }
    }
    if !errs.is_empty() {
        return Err(Error::InvalidModel(errs));
    }
    g.warnings = ctx.warnings;
    Ok(g)
}

/// A normalized identifier (expression language, "Comparison Table"): a regular
/// one upper-cased, a quoted one as written. So `"ID"` matches `id`, and `"id"`
/// doesn't.
pub(super) fn normalized(name: &str, quoted: bool) -> String {
    if quoted {
        name.to_string()
    } else {
        name.to_uppercase()
    }
}

/// A name as the model declares it: `"Name"` in its quotes is a quoted identifier.
pub(super) fn declared(name: &str) -> String {
    match name
        .strip_prefix('"')
        .and_then(|n| n.strip_suffix('"'))
        .filter(|n| !n.is_empty())
    {
        Some(n) => n.replace("\"\"", "\""),
        None => normalized(name, false),
    }
}

/// An identifier that resolves to the name as the model declares it.
pub(super) fn declared_identifier(name: &str) -> Identifier {
    if name.starts_with('"') {
        Identifier::quoted(declared(name))
    } else {
        Identifier::new(name)
    }
}

pub(super) fn source_expr(src: &str, alias: &str) -> std::result::Result<Expression, String> {
    let t = src.trim();
    let upper = t.to_ascii_uppercase();
    if upper.starts_with("SELECT") || upper.starts_with("WITH") || t.starts_with('(') {
        let inner = match t.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
            Some(s) => s,
            None => t,
        };
        let q = sql::parse(inner, DialectType::Generic)?;
        return Ok(sql::derived(q, crate::sql::quoted(alias)));
    }
    if t.is_empty() || t.contains(char::is_whitespace) {
        return Err(format!("`{src}` is neither a table reference nor a query"));
    }
    let mut e = polyglot_sql::builder::table(t).into_inner();
    if let Expression::Table(tr) = &mut e {
        tr.alias = Some(crate::sql::quoted(alias));
    }
    Ok(e)
}

pub(super) fn temporal(datatype: Option<&str>) -> bool {
    datatype.is_some_and(|d| {
        ["date", "time", "datetime", "datetimetz"].contains(&d.to_ascii_lowercase().as_str())
    })
}

pub(super) fn dup_names<'a>(
    names: impl IntoIterator<Item = &'a str>,
    what: &str,
    errs: &mut Vec<String>,
) {
    let mut seen = BTreeSet::new();
    for n in names {
        if !seen.insert(declared(n)) {
            errs.push(format!("duplicate {what} `{n}`"));
        }
    }
}

pub(super) fn check_length(name: &str, what: &str, errs: &mut Vec<String>) {
    if declared(name).chars().count() > MAX_IDENTIFIER {
        errs.push(format!(
            "{what} `{name}` is longer than {MAX_IDENTIFIER} characters"
        ));
    }
}

/// Whether a field is one of those, which nothing but the metric can name.
pub(super) fn is_hidden(field: &str) -> bool {
    field.starts_with(SOURCE_COLUMN)
}

/// `source.column` in a dataset-scoped metric reads a column the dataset may not
/// declare. Each becomes a hidden field, so the rest of the compiler only sees
/// fields. When the dataset declares a field named `source`, the name is that
/// field and the reference is left to fail as an unknown dataset (Name Resolution).
pub(super) fn source_columns(ast: Expression, d: &RefCell<&mut DatasetInfo>) -> Expression {
    let declares_source = lookup(
        d.borrow().fields.iter().map(|f| f.name.as_str()),
        "source",
        false,
    )
    .is_ok_and(|hit| hit.is_some());
    if declares_source {
        return ast;
    }
    transform(ast, &|e| {
        let Expression::Column(c) = e else {
            return Ok(Some(e));
        };
        let is_source = c
            .table
            .as_ref()
            .is_some_and(|t| normalized(&t.name, t.quoted) == "SOURCE");
        if !is_source {
            return Ok(Some(Expression::Column(c)));
        }
        let mut d = d.borrow_mut();
        let mut column = c.clone();
        column.table = None;
        let ast = Expression::Column(column);
        let name = format!("{SOURCE_COLUMN}{}", d.fields.len());
        d.fields.push(FieldInfo {
            name: name.clone(),
            variants: vec![Variant {
                flavor: Flavor::Portable,
                ast,
            }],
            datatype: None,
            is_time: false,
        });
        let mut field = c;
        field.table = None;
        field.name = Identifier::new(name);
        Ok(Some(Expression::Column(field)))
    })
    .expect("naming source columns cannot fail")
}
