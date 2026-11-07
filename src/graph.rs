//! Binding: validate an Ossie model once and resolve its names.
//!
//! The result, a [`SemanticGraph`], holds parsed expressions for every field
//! and metric, the relationship graph, and enough metadata to describe the
//! model to an agent.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use polyglot_sql::expressions::{Column, Identifier};
use polyglot_sql::{
    contains_aggregate, contains_window_function, get_aggregate_functions, get_columns,
    is_aggregate, DialectType, Expression,
};
use serde::Serialize;

use crate::model::{self, Model};
use crate::portable;
use crate::sql::{self, transform, Tpl};
use crate::wide_table::ErrorCode;
use crate::{Error, Result};

/// Which kind of expression a dialect entry carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Flavor {
    /// `expression.sql` or `OSSIE_SQL_2026`.
    Portable,
    /// `ANSI_SQL`.
    Ansi,
    /// A vendor SQL dialect polyglot can read (`SNOWFLAKE`, `TRINO`, ...).
    Vendor(String, DialectType),
}

#[derive(Debug, Clone)]
pub(crate) struct Variant {
    pub flavor: Flavor,
    pub ast: Expression,
}

#[derive(Clone)]
pub(crate) struct DatasetInfo {
    pub name: String,
    pub source: Expression,
    pub fields: Vec<FieldInfo>,
    pub primary_key: Vec<usize>,
}

#[derive(Clone)]
pub(crate) struct FieldInfo {
    pub name: String,
    pub variants: Vec<Variant>,
    pub datatype: Option<String>,
    pub is_time: bool,
}

#[derive(Clone)]
pub(crate) struct RelInfo {
    pub name: String,
    pub from: usize,
    pub to: usize,
    pub from_fields: Vec<usize>,
    pub to_fields: Vec<usize>,
}

#[derive(Clone)]
pub(crate) struct MetricInfo {
    /// `metric` for model-scoped metrics, `dataset.metric` for dataset-scoped ones.
    pub name: String,
    pub owner: Option<usize>,
    pub variants: Vec<Variant>,
    /// Set when the metric is valid Ossie that this version can't compile yet.
    pub unsupported: Option<String>,
}

/// A validated, bound Ossie model, ready to compile requests against.
#[derive(Clone)]
pub struct SemanticGraph {
    pub(crate) model: Model,
    pub(crate) datasets: Vec<DatasetInfo>,
    pub(crate) rels: Vec<RelInfo>,
    pub(crate) metrics: Vec<MetricInfo>,
    pub(crate) warnings: Vec<String>,
}

/// One aggregate inside a metric, with its field references already replaced
/// by qualified field expressions.
#[derive(Debug, Clone)]
pub(crate) struct Term {
    pub ast: Expression,
    pub datasets: BTreeSet<usize>,
    /// `MIN`, `MAX`, `COUNT(DISTINCT ...)` and friends: duplicated input rows can't change them.
    pub dup_insensitive: bool,
    /// `COUNT` and friends: over no rows the answer is 0, not NULL (spec §8.1).
    pub zero_if_empty: bool,
    /// The result's type family (`NUMERIC`, `TEXT`, `DATE`, ...), when known.
    /// Engines that can't type a bare NULL in a UNION need it.
    pub null_type: Option<&'static str>,
    /// Generated SQL of `ast`; identical aggregates across metrics share one term.
    pub key: String,
}

/// One reason wide table §5.2 refuses a query naming a model-scoped metric.
pub(crate) struct MetricRejection {
    pub code: ErrorCode,
    pub message: String,
    /// The spec defers this case, so `deferredMetrics` can ask for it.
    pub deferred: bool,
}

/// A metric split into aggregate terms and an outer expression over them.
/// The outer expression refers to term `i` as the column `__ossiec_t{i}`.
pub(crate) struct ResolvedMetric {
    pub outer: Expression,
    pub terms: Vec<Term>,
    /// Fields a window partitions or orders by; the outer expression refers to
    /// field `i` as `__ossiec_d{i}`, and the request must group by each one.
    pub dims: Vec<(usize, usize)>,
    pub windowed: bool,
}

/// Per-compile state: dialect preference, the vendor dialects actually used, warnings.
#[derive(Default)]
pub(crate) struct Ctx {
    pub prefer: Option<String>,
    pub vendors: BTreeMap<String, DialectType>,
    pub warnings: Vec<String>,
}

impl Ctx {
    fn pick<'a>(&mut self, vs: &'a [Variant]) -> &'a Variant {
        let v = choose(vs, self.prefer.as_deref());
        if let Flavor::Vendor(name, dt) = &v.flavor {
            self.vendors.insert(name.clone(), *dt);
        }
        v
    }

    pub fn warn(&mut self, w: String) {
        if !self.warnings.contains(&w) {
            self.warnings.push(w);
        }
    }

    /// The single dialect the whole statement is written in.
    pub fn source_dialect(&self) -> Result<(String, DialectType)> {
        let mut it = self.vendors.iter();
        match (it.next(), it.next()) {
            (None, _) => Ok(("generic".into(), DialectType::Generic)),
            (Some((n, d)), None) => Ok((n.to_ascii_lowercase(), *d)),
            (Some(_), Some(_)) => Err(Error::Unsupported(format!(
                "this request needs expressions from several vendor dialects ({}); one statement can only be written in one",
                self.vendors.keys().cloned().collect::<Vec<_>>().join(", ")
            ))),
        }
    }
}

/// Preference order: the preferred vendor dialect, then the portable form, then ANSI, then any vendor.
pub(crate) fn choose<'a>(vs: &'a [Variant], prefer: Option<&str>) -> &'a Variant {
    let vendor = |p: &str| {
        vs.iter()
            .find(|v| matches!(&v.flavor, Flavor::Vendor(n, _) if n.eq_ignore_ascii_case(p)))
    };
    prefer
        .and_then(vendor)
        .or_else(|| vs.iter().find(|v| v.flavor == Flavor::Portable))
        .or_else(|| vs.iter().find(|v| v.flavor == Flavor::Ansi))
        .unwrap_or(&vs[0])
}

const NON_SQL: [&str; 7] = [
    "MDX",
    "DAX",
    "TABLEAU",
    "MAQL",
    "SIGMA",
    "THOUGHTSPOT",
    "HOLISTICS_AQL",
];

fn vendor_dialect(name: &str) -> Option<DialectType> {
    if NON_SQL.contains(&name) {
        return None;
    }
    match name.to_ascii_lowercase().parse::<DialectType>() {
        Ok(DialectType::Generic) | Err(_) => None,
        Ok(d) => Some(d),
    }
}

/// `/` in portable expressions is true division on every engine. Postgres and
/// Trino truncate integer division (`7 / 2 = 3`), so a ratio of counts would
/// read 0 there; DuckDB, StarRocks and ClickHouse return 3.5. When neither side
/// is a fractional literal (`x / 100.0` is already exact, and keeps DECIMAL
/// precision for money), the numerator is cast to DOUBLE. A non-literal
/// divisor is wrapped in `NULLIF(…, 0)`, so dividing by zero gives NULL on every engine.
/// Evidence: engine case `integer_ratio`.
pub(crate) fn true_division(ast: Expression) -> Expression {
    use polyglot_sql::expressions::Literal;
    let fractional = |e: &Expression| matches!(e, Expression::Literal(l) if matches!(l.as_ref(), Literal::Number(n) if n.contains(['.', 'e', 'E'])));
    use polyglot_sql::builder::{cast, func, lit, Expr};
    let take = |e: &mut Expression| {
        Expr(std::mem::replace(
            e,
            Expression::Null(polyglot_sql::expressions::Null),
        ))
    };
    transform(ast, &|e| {
        Ok(Some(match e {
            Expression::Div(mut d) => {
                if !fractional(&d.left) && !fractional(&d.right) {
                    d.left = cast(take(&mut d.left), "DOUBLE").into_inner();
                }
                // x / 0 is NULL everywhere, instead of an error (Postgres,
                // MySQL strict) or NaN/Infinity (Trino, ClickHouse doubles).
                let guarded = match &d.right {
                    Expression::Literal(_) | Expression::NullIf(_) | Expression::Nullif(_) => true,
                    Expression::Function(f) => f.name.eq_ignore_ascii_case("NULLIF"),
                    _ => false,
                };
                if !guarded {
                    d.right = func("NULLIF", [take(&mut d.right), lit(0)]).into_inner();
                }
                Expression::Div(d)
            }
            other => other,
        }))
    })
    .expect("division rewrite cannot fail")
}

fn variants(e: &model::Expression, what: &str) -> std::result::Result<Vec<Variant>, String> {
    let mut out = vec![];
    let portable = |text: &str, shown: &str| -> std::result::Result<Expression, String> {
        let ast = sql::parse(&portable::spell_types(text), DialectType::Generic)
            .map_err(|err| format!("{what}: cannot parse {shown}`{text}`: {err}"))?;
        if let Some(why) = portable::refusal(&ast) {
            return Err(format!("{what}: {why}"));
        }
        Ok(true_division(portable::normalize(ast)))
    };
    if let Some(s) = &e.sql {
        out.push(Variant {
            flavor: Flavor::Portable,
            ast: portable(s, "")?,
        });
    }
    for d in &e.dialects {
        let name = d.dialect.to_ascii_uppercase();
        let (flavor, read) = match name.as_str() {
            "OSSIE_SQL_2026" if e.sql.is_some() => continue, // `sql` wins
            "OSSIE_SQL_2026" => (Flavor::Portable, DialectType::Generic),
            "ANSI_SQL" => (Flavor::Ansi, DialectType::Generic),
            _ => match vendor_dialect(&name) {
                Some(dt) => (Flavor::Vendor(name.clone(), dt), dt),
                None => continue,
            },
        };
        if matches!(flavor, Flavor::Portable) {
            out.push(Variant {
                flavor,
                ast: portable(&d.expression, &format!("{name} "))?,
            });
            continue;
        }
        let ast = sql::parse(&d.expression, read)
            .map_err(|err| format!("{what}: cannot parse {name} `{}`: {err}", d.expression))?;
        // Vendor expressions keep their engine's own semantics.
        let ast = if matches!(flavor, Flavor::Vendor(..)) {
            ast
        } else {
            true_division(ast)
        };
        out.push(Variant { flavor, ast });
    }
    if out.is_empty() {
        let declared: Vec<_> = e.dialects.iter().map(|d| d.dialect.as_str()).collect();
        return Err(format!(
            "{what}: no expression in a SQL dialect ossiec can read (declared: {})",
            if declared.is_empty() {
                "none".into()
            } else {
                declared.join(", ")
            }
        ));
    }
    Ok(out)
}

/// A normalized identifier (expression language, "Comparison Table"): a regular
/// one upper-cased, a quoted one as written. So `"ID"` matches `id`, and `"id"`
/// doesn't.
fn normalized(name: &str, quoted: bool) -> String {
    if quoted {
        name.to_string()
    } else {
        name.to_uppercase()
    }
}

/// A name as the model declares it: `"Name"` in its quotes is a quoted identifier.
fn declared(name: &str) -> String {
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
fn declared_identifier(name: &str) -> Identifier {
    if name.starts_with('"') {
        Identifier::quoted(declared(name))
    } else {
        Identifier::new(name)
    }
}

/// Match `wanted` against declared names by their normalized forms.
pub(crate) fn lookup<'a>(
    names: impl IntoIterator<Item = &'a str>,
    wanted: &str,
    quoted: bool,
) -> std::result::Result<Option<usize>, String> {
    // An unquoted `wanted` may be a name from the model, in its own quotes.
    let wanted = if quoted {
        wanted.to_string()
    } else {
        declared(wanted)
    };
    let names: Vec<&str> = names.into_iter().collect();
    let hits: Vec<usize> = (0..names.len())
        .filter(|&i| declared(names[i]) == wanted)
        .collect();
    match hits.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(*one)),
        many => Err(format!(
            "`{wanted}` is ambiguous: it matches {}",
            many.iter()
                .map(|&i| names[i])
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Qualify the bare source columns of a field expression with the dataset alias.
fn qualify(ast: Expression, dataset: &str, alias: &str) -> Expression {
    transform(ast, &|e| {
        Ok(Some(match e {
            Expression::Column(mut c) => {
                if c.table
                    .as_ref()
                    .is_none_or(|t| t.name.eq_ignore_ascii_case(dataset))
                {
                    c.table = Some(crate::sql::quoted(alias));
                }
                Expression::Column(c)
            }
            other => other,
        }))
    })
    .expect("qualifying columns cannot fail")
}

fn source_expr(src: &str, alias: &str) -> std::result::Result<Expression, String> {
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

fn temporal(datatype: Option<&str>) -> bool {
    datatype.is_some_and(|d| {
        ["date", "time", "datetime", "datetimetz"].contains(&d.to_ascii_lowercase().as_str())
    })
}

fn dup_names<'a>(names: impl IntoIterator<Item = &'a str>, what: &str, errs: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    for n in names {
        if !seen.insert(declared(n)) {
            errs.push(format!("duplicate {what} `{n}`"));
        }
    }
}

/// The limit on an identifier's length (expression language, "Namespacing").
const MAX_IDENTIFIER: usize = 128;

fn check_length(name: &str, what: &str, errs: &mut Vec<String>) {
    if declared(name).chars().count() > MAX_IDENTIFIER {
        errs.push(format!(
            "{what} `{name}` is longer than {MAX_IDENTIFIER} characters"
        ));
    }
}

/// The name of a hidden field that reads one source column.
const SOURCE_COLUMN: &str = "__source_";

/// Whether a field is one of those, which nothing but the metric can name.
fn is_hidden(field: &str) -> bool {
    field.starts_with(SOURCE_COLUMN)
}

/// `source.column` in a dataset-scoped metric reads a column the dataset may not
/// declare. Each becomes a hidden field, so the rest of the compiler only sees
/// fields. When the dataset declares a field named `source`, the name is that
/// field and the reference is left to fail as an unknown dataset (Name Resolution).
fn source_columns(ast: Expression, d: &RefCell<&mut DatasetInfo>) -> Expression {
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

/// The `FILTER (WHERE …)` predicate on a typed aggregate, if any.
fn aggregate_filter(e: &Expression) -> Option<&Expression> {
    match e {
        Expression::Sum(f)
        | Expression::Avg(f)
        | Expression::Min(f)
        | Expression::Max(f)
        | Expression::Median(f)
        | Expression::Stddev(f)
        | Expression::StddevPop(f)
        | Expression::StddevSamp(f)
        | Expression::Variance(f)
        | Expression::VarPop(f)
        | Expression::VarSamp(f)
        | Expression::ApproxDistinct(f)
        | Expression::ApproxCountDistinct(f)
        | Expression::AnyValue(f) => f.filter.as_ref(),
        Expression::Count(c) => c.filter.as_ref(),
        Expression::AggregateFunction(a) => a.filter.as_ref(),
        Expression::Filter(f) => Some(&*f.expression),
        _ => None,
    }
}

/// Same aggregate with its FILTER clause removed (for dataset checks on the body).
fn clear_aggregate_filter(e: Expression) -> Expression {
    match e {
        Expression::Sum(mut f) => {
            f.filter = None;
            Expression::Sum(f)
        }
        Expression::Avg(mut f) => {
            f.filter = None;
            Expression::Avg(f)
        }
        Expression::Min(mut f) => {
            f.filter = None;
            Expression::Min(f)
        }
        Expression::Max(mut f) => {
            f.filter = None;
            Expression::Max(f)
        }
        Expression::Median(mut f) => {
            f.filter = None;
            Expression::Median(f)
        }
        Expression::Stddev(mut f) => {
            f.filter = None;
            Expression::Stddev(f)
        }
        Expression::StddevPop(mut f) => {
            f.filter = None;
            Expression::StddevPop(f)
        }
        Expression::StddevSamp(mut f) => {
            f.filter = None;
            Expression::StddevSamp(f)
        }
        Expression::Variance(mut f) => {
            f.filter = None;
            Expression::Variance(f)
        }
        Expression::VarPop(mut f) => {
            f.filter = None;
            Expression::VarPop(f)
        }
        Expression::VarSamp(mut f) => {
            f.filter = None;
            Expression::VarSamp(f)
        }
        Expression::ApproxDistinct(mut f) => {
            f.filter = None;
            Expression::ApproxDistinct(f)
        }
        Expression::ApproxCountDistinct(mut f) => {
            f.filter = None;
            Expression::ApproxCountDistinct(f)
        }
        Expression::AnyValue(mut f) => {
            f.filter = None;
            Expression::AnyValue(f)
        }
        Expression::Count(mut c) => {
            c.filter = None;
            Expression::Count(c)
        }
        Expression::AggregateFunction(mut a) => {
            a.filter = None;
            Expression::AggregateFunction(a)
        }
        Expression::Filter(f) => *f.this,
        other => other,
    }
}

/// Lower `agg(x) FILTER (WHERE p)` to `agg(CASE WHEN p THEN x END)`.
/// The expression language allows this rewrite (`expression_language.md`, FILTER section).
/// Called for targets that lack native aggregate FILTER, and on every target for
/// the statistical aggregates: polyglot's transpile drops their FILTER (`VAR_POP`
/// on DuckDB, Postgres and BigQuery). Evidence: `VAR_POP(v) FILTER (...)` in
/// `expression_language.rs`.
pub(crate) fn lower_filters(e: Expression, only_statistics: bool) -> Result<Expression> {
    fn is_statistical(e: &Expression) -> bool {
        matches!(
            e,
            Expression::Stddev(_)
                | Expression::StddevPop(_)
                | Expression::StddevSamp(_)
                | Expression::Variance(_)
                | Expression::VarPop(_)
                | Expression::VarSamp(_)
        )
    }
    fn case_when(p: Expression, v: Expression) -> Expression {
        let mut t = Tpl::new();
        let (p, v) = (t.e(p), t.e(v));
        t.push(&format!("CASE WHEN {p} THEN {v} END"));
        t.build().expect("constant template")
    }
    macro_rules! lower {
        ($e:expr, $($v:ident),*) => {
            match $e {
                $(Expression::$v(mut f) => {
                    if let Some(p) = f.filter.take() {
                        f.this = case_when(p, f.this);
                    }
                    Expression::$v(f)
                })*
                Expression::Count(mut c) => {
                    if let Some(p) = c.filter.take() {
                        let v = if c.star { sql::col("1") } else { c.this.take().unwrap_or_else(|| sql::col("1")) };
                        c.star = false;
                        c.this = Some(case_when(p, v));
                    }
                    Expression::Count(c)
                }
                Expression::AggregateFunction(mut a) => {
                    if let Some(p) = a.filter.take() {
                        // A constant is a parameter (`APPROX_PERCENTILE(x, 0.5)`), not an input.
                        a.args = a.args.into_iter().map(|x| match x {
                            Expression::Literal(_) => x,
                            x => case_when(p.clone(), x),
                        }).collect();
                    }
                    Expression::AggregateFunction(a)
                }
                Expression::Filter(f) => {
                    let p = *f.expression;
                    match *f.this {
                        Expression::WithinGroup(mut w) => {
                            for o in &mut w.order_by {
                                let v = std::mem::replace(&mut o.this, sql::col("x"));
                                o.this = case_when(p.clone(), v);
                            }
                            Expression::WithinGroup(w)
                        }
                        Expression::ListAgg(mut l) => {
                            l.this = case_when(p, l.this);
                            Expression::ListAgg(l)
                        }
                        other => Expression::Filter(Box::new(polyglot_sql::expressions::Filter {
                            this: Box::new(other),
                            expression: Box::new(p),
                        })),
                    }
                }
                other => other,
            }
        };
    }
    transform(e, &|e| {
        if only_statistics && !is_statistical(&e) {
            return Ok(Some(e));
        }
        Ok(Some(lower!(
            e,
            Sum,
            Avg,
            Min,
            Max,
            Median,
            Stddev,
            StddevPop,
            StddevSamp,
            Variance,
            VarPop,
            VarSamp,
            ApproxDistinct,
            ApproxCountDistinct,
            AnyValue
        )))
    })
    .map_err(|e| Error::Internal(e.to_string()))
}

/// Type family of an aggregate's result: numeric for counting and arithmetic
/// aggregates; for MIN/MAX/ANY_VALUE, the type of the one field they read.
fn null_type(e: &Expression, datatypes: &BTreeSet<Option<String>>) -> Option<&'static str> {
    match e {
        Expression::Min(_) | Expression::Max(_) | Expression::AnyValue(_) => {
            let mut it = datatypes.iter();
            match (it.next(), it.next()) {
                (Some(Some(d)), None) => match d.to_ascii_lowercase().as_str() {
                    "integer" | "decimal" | "float" => Some("NUMERIC"),
                    "string" => Some("TEXT"),
                    "date" => Some("DATE"),
                    "datetime" | "datetimetz" => Some("TIMESTAMP"),
                    "boolean" => Some("BOOLEAN"),
                    _ => None,
                },
                _ => None,
            }
        }
        Expression::AggregateFunction(_) => None,
        _ => Some("NUMERIC"),
    }
}

fn dup_insensitive(e: &Expression) -> bool {
    macro_rules! distinct {
        ($e:expr, $($v:ident),*) => {
            match $e {
                Expression::Min(_) | Expression::Max(_) | Expression::ApproxDistinct(_)
                | Expression::ApproxCountDistinct(_) | Expression::AnyValue(_) => true,
                Expression::Count(c) => c.distinct,
                Expression::AggregateFunction(a) => a.distinct,
                $(Expression::$v(f) => f.distinct,)*
                _ => false,
            }
        };
    }
    distinct!(e, Sum, Avg, Median, Stddev, StddevPop, StddevSamp, Variance, VarPop, VarSamp)
}

pub(crate) const TERM: &str = "__ossiec_t";

pub(crate) const DIM: &str = "__ossiec_d";
const WIN: &str = "__ossiec_w";
const HEAD: &str = "__ossiec_head";

fn is_term_placeholder(c: &Column) -> bool {
    c.table.is_none() && c.name.name.starts_with(TERM)
}

/// `PERCENTILE_CONT(p) WITHIN GROUP (ORDER BY x) FILTER (WHERE q)` as one aggregate
/// over `p, x` with a filter, the way §5.2's checks read every other aggregate.
fn flat_aggregates(ast: Expression) -> Expression {
    transform(ast, &|e| {
        Ok(Some(match e {
            Expression::WithinGroup(w) => match w.this {
                Expression::AggregateFunction(mut a) => {
                    a.args.extend(w.order_by.into_iter().map(|o| o.this));
                    Expression::AggregateFunction(a)
                }
                this => Expression::WithinGroup(Box::new(polyglot_sql::expressions::WithinGroup {
                    this,
                    order_by: w.order_by,
                })),
            },
            Expression::Filter(f) => match *f.this {
                Expression::AggregateFunction(mut a) => {
                    a.filter = Some(*f.expression);
                    Expression::AggregateFunction(a)
                }
                this => Expression::Filter(Box::new(polyglot_sql::expressions::Filter {
                    this: Box::new(this),
                    expression: f.expression,
                })),
            },
            other => other,
        }))
    })
    .expect("flattening aggregates cannot fail")
}

/// Which term a placeholder stands for.
fn term_index(e: &Expression) -> Option<usize> {
    match e {
        Expression::Column(c) if is_term_placeholder(c) => c.name.name[TERM.len()..].parse().ok(),
        _ => None,
    }
}

fn is_placeholder(c: &Column) -> bool {
    c.table.is_none() && c.name.name.starts_with("__ossiec_")
}

/// Apply `f` to the arguments of an aggregate, keeping the aggregate itself.
/// `None` for aggregates ossiec doesn't know how to open.
fn map_agg_args(
    e: Expression,
    f: &dyn Fn(Expression) -> Result<Expression>,
) -> Result<Option<Expression>> {
    macro_rules! open {
        ($($v:ident),*) => {
            match e {
                $(Expression::$v(mut a) => {
                    a.this = f(a.this)?;
                    Expression::$v(a)
                })*
                Expression::Count(mut c) => {
                    if let Some(t) = c.this.take() {
                        c.this = Some(f(t)?);
                    }
                    Expression::Count(c)
                }
                Expression::AggregateFunction(mut a) => {
                    a.args = a.args.into_iter().map(f).collect::<Result<_>>()?;
                    Expression::AggregateFunction(a)
                }
                _ => return Ok(None),
            }
        };
    }
    Ok(Some(open!(
        Sum, Avg, Min, Max, Median, Stddev, StddevPop, StddevSamp, Variance, VarPop, VarSamp,
        AnyValue
    )))
}

impl SemanticGraph {
    /// The bound model.
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Non-fatal findings from binding (for example unqualified references the spec forbids).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub(crate) fn dataset_names(&self) -> String {
        self.datasets
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The field's SQL in the chosen dialect, qualified by its dataset alias.
    pub(crate) fn field_sql(&self, ds: usize, f: usize, ctx: &mut Ctx) -> Expression {
        self.field_sql_as(ds, f, &self.datasets[ds].name, ctx)
    }

    /// The field's SQL, qualified by `alias` (a role alias, or an unmatched-rows alias).
    pub(crate) fn field_sql_as(
        &self,
        ds: usize,
        f: usize,
        alias: &str,
        ctx: &mut Ctx,
    ) -> Expression {
        let d = &self.datasets[ds];
        // polyglot parenthesizes by precedence when generating, so no Paren node is needed.
        qualify(ctx.pick(&d.fields[f].variants).ast.clone(), &d.name, alias)
    }

    /// The dataset's `FROM` item under `alias`.
    pub(crate) fn source_as(&self, ds: usize, alias: &str) -> Expression {
        let mut e = self.datasets[ds].source.clone();
        match &mut e {
            Expression::Table(t) => t.alias = Some(crate::sql::quoted(alias)),
            Expression::Subquery(s) => s.alias = Some(crate::sql::quoted(alias)),
            _ => {}
        }
        e
    }

    /// Resolve `dataset.field`, or `relationship.field` for the dataset that
    /// relationship leads to, playing that role (ANSI identifier rules).
    /// Returns (dataset, field, role relationship).
    pub(crate) fn field_by_name(&self, name: &str) -> Result<(usize, usize, Option<usize>)> {
        match sql::parse(name, DialectType::Generic) {
            Ok(Expression::Column(c)) if c.table.is_some() => self.field_of_column(&c),
            _ => Err(Error::Request(format!(
                "`{name}` is not a field reference; use `dataset.field` or `relationship.field`"
            ))),
        }
    }

    /// [`Self::field_by_name`] for a parsed, qualified column.
    pub(crate) fn field_of_column(&self, c: &Column) -> Result<(usize, usize, Option<usize>)> {
        let Some(t) = c.table.as_ref() else {
            return Err(Error::Request(format!(
                "`{}` is not a field reference; use `dataset.field` or `relationship.field`",
                c.name.name
            )));
        };
        let is_dataset = lookup(
            self.datasets.iter().map(|d| d.name.as_str()),
            &t.name,
            t.quoted,
        )
        .ok()
        .flatten()
        .is_some();
        let role = if is_dataset {
            None
        } else {
            lookup(self.rels.iter().map(|r| r.name.as_str()), &t.name, t.quoted)
                .map_err(Error::Request)?
        };
        let c = match role {
            Some(r) => {
                let mut c = c.clone();
                c.table = Some(declared_identifier(&self.datasets[self.rels[r].to].name));
                c
            }
            None => c.clone(),
        };
        let (ds, f) = self
            .resolve_column(&c, None, &mut Ctx::default())
            .map_err(|e| match e {
                Error::InvalidModel(v) => Error::Request(v.join("; ")),
                other => other,
            })?;
        if is_hidden(&self.datasets[ds].fields[f].name) {
            return Err(Error::Request(format!(
                "`{}` is not a field of `{}`",
                c.name.name, self.datasets[ds].name
            )));
        }
        Ok((ds, f, role))
    }

    /// A dataset-scoped metric that exists only for this compile (a query-local
    /// `SUM|COUNT|AVG|MIN|MAX(...) TO MEASURE`). `name` is `dataset.metric`.
    /// The bound model is left unchanged; callers install on a clone.
    pub(crate) fn install_ephemeral_metric(
        &mut self,
        name: &str,
        owner: usize,
        ast: Expression,
    ) -> Result<()> {
        if self
            .metrics
            .iter()
            .any(|m| m.name.eq_ignore_ascii_case(name))
        {
            return Err(Error::Internal(format!(
                "ephemeral metric `{name}` already exists"
            )));
        }
        self.metrics.push(MetricInfo {
            name: name.to_string(),
            owner: Some(owner),
            variants: vec![Variant {
                flavor: Flavor::Portable,
                ast: true_division(ast),
            }],
            unsupported: None,
        });
        let i = self.metrics.len() - 1;
        match self.resolve_metric_unchecked(i, &mut Ctx::default()) {
            Ok(_) => Ok(()),
            Err(e) => {
                self.metrics.pop();
                Err(match e {
                    Error::InvalidModel(v) => Error::Request(format!(
                        "Layer 2 TO MEASURE base aggregate: {}",
                        v.join("; ")
                    )),
                    Error::Unsupported(why) => {
                        Error::Unsupported(format!("Layer 2 TO MEASURE base aggregate: {why}"))
                    }
                    other => other,
                })
            }
        }
    }

    pub(crate) fn metric_by_name(&self, name: &str) -> Result<usize> {
        match lookup(self.metrics.iter().map(|m| m.name.as_str()), name, false) {
            Ok(Some(i)) => Ok(i),
            Ok(None) => Err(Error::Request(format!(
                "unknown metric `{name}`; available: {}",
                self.metrics
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
            Err(e) => Err(Error::Request(e)),
        }
    }

    /// `FILTER (WHERE p)` may only name fields of the aggregate's own dataset
    /// (`expression_language.md`, Conditional Aggregations).
    fn check_filter_datasets(
        &self,
        agg: &Expression,
        owner: Option<usize>,
        ctx: &mut Ctx,
    ) -> Result<()> {
        let Some(pred) = aggregate_filter(agg) else {
            return Ok(());
        };
        let body = clear_aggregate_filter(agg.clone());
        let body_ds = self.column_datasets(&body, owner, ctx)?;
        let filter_ds = self.column_datasets(pred, owner, ctx)?;
        let mut allowed = body_ds;
        if allowed.is_empty() {
            if let Some(o) = owner {
                allowed.insert(o);
            }
        }
        for &ds in &filter_ds {
            if !allowed.contains(&ds) {
                let names = |s: &BTreeSet<usize>| {
                    s.iter()
                        .map(|&i| self.datasets[i].name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                return Err(Error::InvalidModel(vec![format!(
                    "FILTER (WHERE …) may only reference fields of the aggregate's dataset(s) ({}); predicate uses `{}`",
                    if allowed.is_empty() {
                        "none".into()
                    } else {
                        names(&allowed)
                    },
                    self.datasets[ds].name
                )]));
            }
        }
        Ok(())
    }

    fn column_datasets(
        &self,
        e: &Expression,
        owner: Option<usize>,
        ctx: &mut Ctx,
    ) -> Result<BTreeSet<usize>> {
        let mut out = BTreeSet::new();
        for c in get_columns(e) {
            let Expression::Column(col) = c else {
                continue;
            };
            if is_placeholder(col) {
                continue;
            }
            let (ds, _) = self.resolve_column(col, owner, ctx)?;
            out.insert(ds);
        }
        Ok(out)
    }

    fn resolve_column(
        &self,
        c: &Column,
        owner: Option<usize>,
        ctx: &mut Ctx,
    ) -> Result<(usize, usize)> {
        let bad = |m: String| Error::InvalidModel(vec![m]);
        let ds = match (&c.table, owner) {
            (Some(t), _) => {
                let ds = lookup(
                    self.datasets.iter().map(|d| d.name.as_str()),
                    &t.name,
                    t.quoted,
                )
                .map_err(bad)?
                .ok_or_else(|| {
                    bad(format!(
                        "unknown dataset `{}`; datasets: {}",
                        t.name,
                        self.dataset_names()
                    ))
                })?;
                if let Some(o) = owner.filter(|&o| o != ds) {
                    return Err(bad(format!(
                        "`{}.{}`: a dataset-scoped metric can only reference fields of its own dataset `{}`",
                        t.name, c.name.name, self.datasets[o].name
                    )));
                }
                ds
            }
            (None, Some(o)) => o,
            (None, None) => {
                let hits: Vec<usize> = (0..self.datasets.len())
                    .filter(|&d| {
                        lookup(
                            self.datasets[d].fields.iter().map(|f| f.name.as_str()),
                            &c.name.name,
                            c.name.quoted,
                        )
                        .ok()
                        .flatten()
                        .is_some()
                    })
                    .collect();
                match hits.as_slice() {
                    [one] => {
                        ctx.warn(format!(
                            "unqualified `{}` resolved to `{}.{}`; Ossie requires `dataset.field` in model-scoped metrics",
                            c.name.name, self.datasets[*one].name, c.name.name
                        ));
                        *one
                    }
                    [] => {
                        return Err(bad(format!(
                            "unknown field `{}`; qualify it as `dataset.field`",
                            c.name.name
                        )))
                    }
                    _ => {
                        return Err(bad(format!(
                            "`{}` exists in several datasets; qualify it as `dataset.field`",
                            c.name.name
                        )))
                    }
                }
            }
        };
        let fields = &self.datasets[ds].fields;
        let f = lookup(
            fields.iter().map(|f| f.name.as_str()),
            &c.name.name,
            c.name.quoted,
        )
        .map_err(bad)?
        .ok_or_else(|| {
            bad(format!(
                "`{}` is not a field of `{}`; fields: {}",
                c.name.name,
                self.datasets[ds].name,
                fields
                    .iter()
                    .map(|f| f.name.as_str())
                    .filter(|n| !is_hidden(n))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        Ok((ds, f))
    }

    /// What wide table §5.2 refuses in a model-scoped metric, every case that
    /// applies. Names that don't resolve are an invalid model, whatever else the
    /// metric does.
    pub(crate) fn metric_rejections(
        &self,
        m: usize,
        ctx: &mut Ctx,
    ) -> Result<Vec<MetricRejection>> {
        let info = &self.metrics[m];
        if info.owner.is_some() {
            return Ok(vec![]);
        }
        let name = &info.name;
        let ast = flat_aggregates(ctx.pick(&info.variants).ast.clone());
        self.column_datasets(&ast, None, ctx).map_err(|e| match e {
            Error::InvalidModel(v) => {
                Error::InvalidModel(vec![format!("metric `{name}`: {}", v.join("; "))])
            }
            other => other,
        })?;
        let mut out = vec![];
        // §5.2 reports a window's own clauses under the window, not as stray fields.
        let outside = transform(ast.clone(), &|e| {
            Ok(Some(
                if is_aggregate(&e) || matches!(e, Expression::WindowFunction(_)) {
                    Expression::Null(polyglot_sql::expressions::Null)
                } else {
                    e
                },
            ))
        })
        .map_err(|e| Error::Internal(e.to_string()))?;
        if !get_columns(&outside).is_empty() {
            out.push(MetricRejection {
                code: ErrorCode::UnaggregatedFinerGrainReference,
                message: format!("metric `{name}` reads a field outside every aggregate"),
                deferred: false,
            });
        }
        if contains_window_function(&ast) {
            out.push(MetricRejection {
                code: ErrorCode::DeferredWindowedMetric,
                message: format!(
                    "metric `{name}` contains a window function, which the spec defers; set `deferredMetrics` to compute it over the grouped result"
                ),
                deferred: true,
            });
        }
        for agg in get_aggregate_functions(&ast) {
            let reads = self.column_datasets(&clear_aggregate_filter(agg.clone()), None, ctx)?;
            let filter_reads = match aggregate_filter(agg) {
                Some(p) => self.column_datasets(p, None, ctx)?,
                None => BTreeSet::new(),
            };
            let found = if reads.is_empty() {
                Some((
                    "has an aggregate whose arguments read no dataset; make it a dataset-scoped metric",
                    false,
                ))
            } else if reads.len() > 1 {
                Some((
                    "has an aggregate whose arguments read several datasets, which the spec defers; set `deferredMetrics` to compute it from the dataset that reaches the others",
                    true,
                ))
            } else if !filter_reads.is_subset(&reads) {
                Some((
                    "has a FILTER that reads a dataset other than the one its aggregate reads",
                    false,
                ))
            } else {
                None
            };
            if let Some((why, deferred)) = found {
                out.push(MetricRejection {
                    code: ErrorCode::AmbiguousMeasureGrain,
                    message: format!("metric `{name}` {why}"),
                    deferred,
                });
            }
        }
        Ok(out)
    }

    pub(crate) fn resolve_metric(&self, m: usize, ctx: &mut Ctx) -> Result<ResolvedMetric> {
        if let Some(why) = &self.metrics[m].unsupported {
            return Err(Error::Unsupported(format!(
                "metric `{}`: {why}",
                self.metrics[m].name
            )));
        }
        self.resolve_metric_unchecked(m, ctx)
    }

    fn resolve_metric_unchecked(&self, m: usize, ctx: &mut Ctx) -> Result<ResolvedMetric> {
        let info = &self.metrics[m];
        let bad = |why: &str| Error::InvalidModel(vec![format!("metric `{}`: {why}", info.name)]);
        // Keep FILTER on the AST until compile knows the target dialect. Engines
        // that support it get the idiomatic form; others get a CASE rewrite.
        let ast = ctx.pick(&info.variants).ast.clone();
        let internal = |e: polyglot_sql::Error| Error::Internal(e.to_string());

        // Window functions run over the grouped result, after the aggregates.
        // Set them aside so their own aggregate (the outer SUM of
        // `SUM(SUM(x)) OVER (...)`) isn't lifted as a term.
        let windows = RefCell::new(Vec::new());
        let ast = transform(ast, &|e| {
            Ok(Some(match e {
                Expression::WindowFunction(_) => {
                    let mut w = windows.borrow_mut();
                    w.push(e);
                    sql::col(&format!("{WIN}{}", w.len() - 1))
                }
                other => other,
            }))
        })
        .map_err(internal)?;

        // Lift every aggregate into a term placeholder.
        let lifted = RefCell::new(Vec::new());
        let lift = |e: Expression| {
            transform(e, &|e| {
                let mut l = lifted.borrow_mut();
                // `PERCENTILE_CONT(p) WITHIN GROUP (...)`, and a FILTER on one, wrap
                // an aggregate already lifted: the wrapper belongs in its term.
                let wrapped = match &e {
                    Expression::WithinGroup(w) => term_index(&w.this),
                    Expression::Filter(f) => term_index(&f.this),
                    _ => None,
                };
                if let Some(i) = wrapped {
                    let inner = std::mem::replace(&mut l[i], sql::col("x"));
                    l[i] = match e {
                        Expression::WithinGroup(mut w) => {
                            w.this = inner;
                            Expression::WithinGroup(w)
                        }
                        Expression::Filter(mut f) => {
                            f.this = Box::new(inner);
                            Expression::Filter(f)
                        }
                        other => other,
                    };
                    return Ok(Some(sql::col(&format!("{TERM}{i}"))));
                }
                if !is_aggregate(&e) {
                    return Ok(Some(e));
                }
                l.push(e);
                Ok(Some(sql::col(&format!("{TERM}{}", l.len() - 1))))
            })
            .map_err(internal)
        };
        let mut outer = lift(ast)?;
        if lifted.borrow().is_empty() && windows.borrow().is_empty() {
            return Err(bad("a metric must aggregate (SUM, COUNT, AVG, ...)"));
        }
        let has_windows = !windows.borrow().is_empty();
        if !has_windows
            && get_columns(&outer)
                .iter()
                .any(|c| matches!(c, Expression::Column(c) if !is_placeholder(c)))
        {
            return Err(bad("references a field outside an aggregate"));
        }

        // Inside a window, aggregates become terms and the remaining fields
        // must be requested dimensions: they become `__ossiec_d{n}` placeholders.
        let dims = RefCell::new(Vec::<(usize, usize)>::new());
        let ctx_cell = RefCell::new(&mut *ctx);
        let as_dims = |e: Expression| -> Result<Expression> {
            let failure = RefCell::new(None);
            let out = transform(e, &|e| {
                let Expression::Column(c) = e else {
                    return Ok(Some(e));
                };
                if is_placeholder(&c) {
                    return Ok(Some(Expression::Column(c)));
                }
                match self.resolve_column(&c, info.owner, &mut ctx_cell.borrow_mut()) {
                    Ok((ds, f)) if is_hidden(&self.datasets[ds].fields[f].name) => {
                        failure.borrow_mut().get_or_insert(Error::Unsupported(format!(
                            "metric `{}`: a window reads the grouped result, which has no source columns; declare a field for the column it reads",
                            info.name
                        )));
                        Ok(Some(Expression::Column(c)))
                    }
                    Ok(df) => {
                        let mut d = dims.borrow_mut();
                        let i = d.iter().position(|x| *x == df).unwrap_or_else(|| {
                            d.push(df);
                            d.len() - 1
                        });
                        Ok(Some(sql::col(&format!("{DIM}{i}"))))
                    }
                    Err(err) => {
                        failure.borrow_mut().get_or_insert(err);
                        Ok(Some(Expression::Column(c)))
                    }
                }
            })
            .map_err(internal)?;
            failure.into_inner().map_or(Ok(out), Err)
        };
        let mut resolved_windows = vec![];
        for w in windows.into_inner() {
            let Expression::WindowFunction(mut wf) = w else {
                unreachable!()
            };
            let head = std::mem::replace(&mut wf.this, sql::col(HEAD));
            let head = if is_aggregate(&head) {
                map_agg_args(head, &lift)?.ok_or_else(|| {
                    Error::Unsupported(
                        "this aggregate can't be used as a window function yet".into(),
                    )
                })?
            } else {
                lift(head)?
            };
            let (head, body) = (
                as_dims(head)?,
                as_dims(lift(Expression::WindowFunction(wf))?)?,
            );
            if contains_window_function(&head)
                || get_columns(&head)
                    .iter()
                    .any(|c| matches!(c, Expression::Column(c) if c.name.name.starts_with(WIN)))
            {
                return Err(bad("window functions can't be nested"));
            }
            resolved_windows.push(
                transform(body, &|e| {
                    Ok(Some(match e {
                        Expression::Column(c) if c.name.name == HEAD => head.clone(),
                        other => other,
                    }))
                })
                .map_err(internal)?,
            );
        }
        let windowed = !resolved_windows.is_empty();
        if windowed {
            // Beside a window, a field is a column of the grouped result, like the
            // ones the window itself reads: `amount / SUM(amount) OVER ()`.
            outer = as_dims(outer)?;
            outer = transform(outer, &|e| {
                Ok(Some(match e {
                    Expression::Column(c) if c.table.is_none() && c.name.name.starts_with(WIN) => {
                        let i: usize = c.name.name[WIN.len()..]
                            .parse()
                            .expect("window placeholder");
                        resolved_windows[i].clone()
                    }
                    other => other,
                }))
            })
            .map_err(internal)?;
        }
        let dims = dims.into_inner();
        let mut lifted = lifted.into_inner();
        // `ROW_NUMBER() OVER (...)` reads no aggregate. A term no expression uses
        // still gives the metric its home dataset, whose rows it runs over.
        let no_terms = lifted.is_empty();
        if no_terms {
            lifted.push(sql::parse("COUNT(*)", DialectType::Generic).map_err(Error::Internal)?);
        }

        let mut terms = vec![];
        for raw in lifted {
            let nested = get_aggregate_functions(&raw).len() > 1
                || get_columns(&raw)
                    .iter()
                    .any(|c| matches!(c, Expression::Column(c) if is_term_placeholder(c)));
            if nested {
                return Err(bad("aggregates can only be nested inside a window function (SUM(SUM(x)) OVER (...))"));
            }
            // expression_language.md: FILTER predicates may only reference fields
            // of the same dataset as the aggregate's arguments.
            self.check_filter_datasets(&raw, info.owner, &mut ctx_cell.borrow_mut())
                .map_err(|e| match e {
                    Error::InvalidModel(v) => bad(&v.join("; ")),
                    other => other,
                })?;
            let datasets = RefCell::new(BTreeSet::new());
            let datatypes = RefCell::new(BTreeSet::new());
            let failure = RefCell::new(None);
            let ast = transform(raw.clone(), &|e| {
                let Expression::Column(c) = e else {
                    return Ok(Some(e));
                };
                let mut ctx = ctx_cell.borrow_mut();
                Ok(Some(match self.resolve_column(&c, info.owner, &mut ctx) {
                    Ok((ds, f)) => {
                        datasets.borrow_mut().insert(ds);
                        datatypes
                            .borrow_mut()
                            .insert(self.datasets[ds].fields[f].datatype.clone());
                        self.field_sql(ds, f, &mut ctx)
                    }
                    Err(err) => {
                        failure.borrow_mut().get_or_insert(err);
                        Expression::Column(c)
                    }
                }))
            })
            .map_err(|e| Error::Internal(e.to_string()))?;
            if let Some(err) = failure.into_inner() {
                return Err(err);
            }
            let mut datasets = datasets.into_inner();
            if datasets.is_empty() {
                match info.owner.or_else(|| dims.first().filter(|_| no_terms).map(|d| d.0)) {
                    Some(o) => {
                        datasets.insert(o);
                    }
                    None if no_terms => return Err(bad("a window that reads no field has no dataset to run over; make it a dataset-scoped metric")),
                    None => return Err(bad("an aggregate without field references (like COUNT(*)) must be a dataset-scoped metric, or use COUNT(dataset.key)")),
                }
            }
            let key = polyglot_sql::generate(&ast, DialectType::Generic)
                .map_err(|e| Error::Internal(e.to_string()))?;
            let datatypes = datatypes.into_inner();
            terms.push(Term {
                null_type: null_type(&raw, &datatypes),
                dup_insensitive: dup_insensitive(&raw),
                zero_if_empty: matches!(
                    raw,
                    Expression::Count(_)
                        | Expression::CountIf(_)
                        | Expression::ApproxDistinct(_)
                        | Expression::ApproxCountDistinct(_)
                ),
                ast,
                datasets,
                key,
            });
        }
        Ok(ResolvedMetric {
            outer,
            terms,
            dims,
            windowed,
        })
    }

    /// Every simple path from `root` along many-to-one relationships, by target.
    pub(crate) fn paths_from(&self, root: usize) -> BTreeMap<usize, Vec<Vec<usize>>> {
        fn walk(
            g: &SemanticGraph,
            at: usize,
            path: &mut Vec<usize>,
            seen: &mut Vec<usize>,
            out: &mut BTreeMap<usize, Vec<Vec<usize>>>,
        ) {
            for (ri, r) in g.rels.iter().enumerate() {
                if r.from != at || seen.contains(&r.to) {
                    continue;
                }
                path.push(ri);
                seen.push(r.to);
                let found = out.entry(r.to).or_default();
                if found.len() < 16 {
                    found.push(path.clone());
                    walk(g, r.to, path, seen, out);
                }
                seen.pop();
                path.pop();
            }
        }
        let mut out = BTreeMap::new();
        walk(self, root, &mut vec![], &mut vec![root], &mut out);
        out
    }

    fn required_dims(&self, metric: &str) -> Vec<String> {
        let Ok(i) = self.metric_by_name(metric) else {
            return vec![];
        };
        let Ok(rm) = self.resolve_metric(i, &mut Ctx::default()) else {
            return vec![];
        };
        rm.dims
            .iter()
            .map(|&(d, f)| {
                format!(
                    "{}.{}",
                    self.datasets[d].name, self.datasets[d].fields[f].name
                )
            })
            .collect()
    }

    /// A description of everything a caller may ask for, for agents and UIs.
    pub fn describe(&self) -> Catalog {
        let mut dimensions = vec![];
        for (d, ds) in self.model.datasets.iter().zip(&self.datasets) {
            for (f, fi) in d.fields.iter().zip(&ds.fields) {
                dimensions.push(DimensionDoc {
                    name: format!("{}.{}", d.name, f.name),
                    label: f.label.clone(),
                    description: f.description.clone(),
                    datatype: f.datatype.clone(),
                    is_time: fi.is_time,
                    declared_dimension: f.dimension.is_some(),
                    ai_context: f.ai_context.clone(),
                    dialects: dialect_names(&fi.variants),
                });
            }
        }
        let scoped = self.model.datasets.iter().flat_map(|d| d.metrics.iter());
        let metrics = scoped
            .chain(self.model.metrics.iter())
            .zip(&self.metrics)
            .enumerate()
            .map(|(i, (m, mi))| MetricDoc {
                name: mi.name.clone(),
                description: m.description.clone(),
                datatype: m.datatype.clone(),
                ai_context: m.ai_context.clone(),
                queryable: mi.unsupported.is_none(),
                not_queryable_because: mi.unsupported.clone(),
                requires_dimensions: self.required_dims(&mi.name),
                requires_deferred_metrics: self
                    .metric_rejections(i, &mut Ctx::default())
                    .is_ok_and(|r| r.iter().any(|r| r.deferred)),
                dialects: dialect_names(&mi.variants),
            })
            .collect();
        Catalog {
            name: self.model.name.clone(),
            version: self.model.version.clone(),
            description: self.model.description.clone(),
            ai_context: self.model.ai_context.clone(),
            metrics,
            dimensions,
            relationships: self
                .rels
                .iter()
                .map(|r| {
                    format!(
                        "{}: {} → {}",
                        r.name, self.datasets[r.from].name, self.datasets[r.to].name
                    )
                })
                .collect(),
        }
    }
}

fn dialect_names(vs: &[Variant]) -> Vec<String> {
    vs.iter()
        .map(|v| match &v.flavor {
            Flavor::Portable => "OSSIE_SQL_2026".to_string(),
            Flavor::Ansi => "ANSI_SQL".to_string(),
            Flavor::Vendor(n, _) => n.clone(),
        })
        .collect()
}

/// What a model offers, as returned by [`SemanticGraph::describe`].
#[derive(Debug, Serialize)]
pub struct Catalog {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub ai_context: Option<serde_json::Value>,
    pub metrics: Vec<MetricDoc>,
    pub dimensions: Vec<DimensionDoc>,
    pub relationships: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct MetricDoc {
    pub name: String,
    pub description: Option<String>,
    pub datatype: Option<String>,
    pub ai_context: Option<serde_json::Value>,
    pub queryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_queryable_because: Option<String>,
    /// Dimensions a window metric partitions or orders by; requests must include them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub requires_dimensions: Vec<String>,
    /// The spec defers this metric: a request must set `deferredMetrics` to get it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub requires_deferred_metrics: bool,
    pub dialects: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DimensionDoc {
    pub name: String,
    pub label: Option<String>,
    pub description: Option<String>,
    pub datatype: Option<String>,
    pub is_time: bool,
    /// The field declares a `dimension` block. Any field can be grouped by.
    pub declared_dimension: bool,
    pub ai_context: Option<serde_json::Value>,
    pub dialects: Vec<String>,
}
