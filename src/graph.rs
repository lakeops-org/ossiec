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
    is_aggregate, transform, DialectType, Expression,
};
use serde::Serialize;

use crate::model::{self, Model};
use crate::sql::{self, Tpl};
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

pub(crate) struct DatasetInfo {
    pub name: String,
    pub source: Expression,
    pub fields: Vec<FieldInfo>,
    pub primary_key: Vec<usize>,
}

pub(crate) struct FieldInfo {
    pub name: String,
    pub variants: Vec<Variant>,
    pub datatype: Option<String>,
    pub is_time: bool,
}

pub(crate) struct RelInfo {
    pub name: String,
    pub from: usize,
    pub to: usize,
    pub from_fields: Vec<usize>,
    pub to_fields: Vec<usize>,
}

pub(crate) struct MetricInfo {
    /// `metric` for model-scoped metrics, `dataset.metric` for dataset-scoped ones.
    pub name: String,
    pub owner: Option<usize>,
    pub variants: Vec<Variant>,
    /// Set when the metric is valid Ossie that this version can't compile yet.
    pub unsupported: Option<String>,
}

/// A validated, bound Ossie model, ready to compile requests against.
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
    /// Generated SQL of `ast`; identical aggregates across metrics share one term.
    pub key: String,
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
/// precision for money), the numerator is cast to DOUBLE.
/// Evidence: engine case `integer_ratio`.
fn true_division(ast: Expression) -> Expression {
    use polyglot_sql::expressions::Literal;
    let fractional = |e: &Expression| matches!(e, Expression::Literal(l) if matches!(l.as_ref(), Literal::Number(n) if n.contains(['.', 'e', 'E'])));
    transform(ast, &|e| {
        Ok(Some(match e {
            Expression::Div(mut d) if !fractional(&d.left) && !fractional(&d.right) => {
                let left = std::mem::replace(
                    &mut d.left,
                    Expression::Null(polyglot_sql::expressions::Null),
                );
                d.left = polyglot_sql::builder::cast(polyglot_sql::builder::Expr(left), "DOUBLE")
                    .into_inner();
                Expression::Div(d)
            }
            other => other,
        }))
    })
    .expect("division rewrite cannot fail")
}

fn variants(e: &model::Expression, what: &str) -> std::result::Result<Vec<Variant>, String> {
    let mut out = vec![];
    if let Some(s) = &e.sql {
        let ast = sql::parse(s, DialectType::Generic)
            .map_err(|err| format!("{what}: cannot parse `{s}`: {err}"))?;
        out.push(Variant {
            flavor: Flavor::Portable,
            ast: true_division(ast),
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

/// ANSI identifier matching: a quoted name matches exactly, a bare one case-insensitively.
pub(crate) fn lookup<'a>(
    names: impl IntoIterator<Item = &'a str>,
    wanted: &str,
    quoted: bool,
) -> std::result::Result<Option<usize>, String> {
    let names: Vec<&str> = names.into_iter().collect();
    if quoted {
        return Ok(names.iter().position(|n| *n == wanted));
    }
    let hits: Vec<usize> = (0..names.len())
        .filter(|&i| names[i].eq_ignore_ascii_case(wanted))
        .collect();
    match hits.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(*one)),
        many => match many.iter().find(|&&i| names[i] == wanted) {
            Some(&i) => Ok(Some(i)),
            None => Err(format!(
                "`{wanted}` is ambiguous: it matches {}",
                many.iter()
                    .map(|&i| names[i])
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        },
    }
}

/// Qualify the bare source columns of a field expression with the dataset alias.
fn qualify(ast: Expression, dataset: &str) -> Expression {
    transform(ast, &|e| {
        Ok(Some(match e {
            Expression::Column(mut c) => {
                if c.table
                    .as_ref()
                    .is_none_or(|t| t.name.eq_ignore_ascii_case(dataset))
                {
                    c.table = Some(Identifier::quoted(dataset));
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
        return Ok(sql::derived(q, Identifier::quoted(alias)));
    }
    if t.is_empty() || t.contains(char::is_whitespace) {
        return Err(format!("`{src}` is neither a table reference nor a query"));
    }
    let mut e = polyglot_sql::builder::table(t).into_inner();
    if let Expression::Table(tr) = &mut e {
        tr.alias = Some(Identifier::quoted(alias));
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
        if !seen.insert(n.to_ascii_lowercase()) {
            errs.push(format!("duplicate {what} `{n}`"));
        }
    }
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
    for d in &model.datasets {
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
        match variants(&m.expression, &format!("metric `{name}`")) {
            Ok(vs) => metrics.push(MetricInfo {
                name,
                owner,
                variants: vs,
                unsupported: None,
            }),
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
        match g.resolve_metric_unchecked(i, &mut ctx) {
            Ok(_) => {}
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

/// Lower `agg(x) FILTER (WHERE p)` to `agg(CASE WHEN p THEN x END)`, which every engine runs.
/// The expression language allows this rewrite (`expression_language.md`, FILTER section).
fn lower_filters(e: Expression) -> Result<Expression> {
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
                        a.args = a.args.into_iter().map(|x| case_when(p.clone(), x)).collect();
                    }
                    Expression::AggregateFunction(a)
                }
                other => other,
            }
        };
    }
    transform(e, &|e| {
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
        let d = &self.datasets[ds];
        // polyglot parenthesizes by precedence when generating, so no Paren node is needed.
        qualify(ctx.pick(&d.fields[f].variants).ast.clone(), &d.name)
    }

    /// Resolve `dataset.field` (ANSI identifier rules on both parts).
    pub(crate) fn field_by_name(&self, name: &str) -> Result<(usize, usize)> {
        let c = match sql::parse(name, DialectType::Generic) {
            Ok(Expression::Column(c)) if c.table.is_some() => c,
            _ => {
                return Err(Error::Request(format!(
                    "`{name}` is not a field reference; use `dataset.field`"
                )))
            }
        };
        self.resolve_column(&c, None, &mut Ctx::default())
            .map_err(|e| match e {
                Error::InvalidModel(v) => Error::Request(v.join("; ")),
                other => other,
            })
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
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        Ok((ds, f))
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
        let ast = lower_filters(ctx.pick(&info.variants).ast.clone())?;
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
                if !is_aggregate(&e) {
                    return Ok(Some(e));
                }
                let mut l = lifted.borrow_mut();
                l.push(e);
                Ok(Some(sql::col(&format!("{TERM}{}", l.len() - 1))))
            })
            .map_err(internal)
        };
        let mut outer = lift(ast)?;
        if lifted.borrow().is_empty() && windows.borrow().is_empty() {
            return Err(bad("a metric must aggregate (SUM, COUNT, AVG, ...)"));
        }
        if get_columns(&outer)
            .iter()
            .any(|c| matches!(c, Expression::Column(c) if !is_placeholder(c)))
        {
            return Err(bad("references a field outside an aggregate (only window PARTITION BY / ORDER BY may name fields directly)"));
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
        let lifted = lifted.into_inner();
        if lifted.is_empty() {
            return Err(bad("a metric must aggregate (SUM, COUNT, AVG, ...)"));
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
            let datasets = RefCell::new(BTreeSet::new());
            let failure = RefCell::new(None);
            let ast = transform(raw.clone(), &|e| {
                let Expression::Column(c) = e else {
                    return Ok(Some(e));
                };
                let mut ctx = ctx_cell.borrow_mut();
                Ok(Some(match self.resolve_column(&c, info.owner, &mut ctx) {
                    Ok((ds, f)) => {
                        datasets.borrow_mut().insert(ds);
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
                match info.owner {
                    Some(o) => {
                        datasets.insert(o);
                    }
                    None => return Err(bad("an aggregate without field references (like COUNT(*)) must be a dataset-scoped metric, or use COUNT(dataset.key)")),
                }
            }
            let key = polyglot_sql::generate(&ast, DialectType::Generic)
                .map_err(|e| Error::Internal(e.to_string()))?;
            terms.push(Term {
                dup_insensitive: dup_insensitive(&raw),
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
            .map(|(m, mi)| MetricDoc {
                name: mi.name.clone(),
                description: m.description.clone(),
                datatype: m.datatype.clone(),
                ai_context: m.ai_context.clone(),
                queryable: mi.unsupported.is_none(),
                not_queryable_because: mi.unsupported.clone(),
                requires_dimensions: self.required_dims(&mi.name),
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
