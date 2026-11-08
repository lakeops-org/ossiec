//! Binding: validate an Ossie model once and resolve its names.
//!
//! The result, a [`SemanticGraph`], holds parsed expressions for every field
//! and metric, the relationship graph, and enough metadata to describe the
//! model to an agent.

use std::collections::{BTreeMap, BTreeSet};

use polyglot_sql::expressions::Column;
use polyglot_sql::{DialectType, Expression};

use crate::model::{self, Model};
use crate::portable;
use crate::sql::{self, transform};
use crate::wide_table::ErrorCode;
use crate::{Error, Result};

mod bind;
mod describe;
mod metric;

pub use bind::bind;
use bind::{declared, declared_identifier, is_hidden};
pub use describe::{Catalog, DimensionDoc, MetricDoc};

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
    /// A Layer 2 plain SQL aggregate (§7.2), not a measure: see [`Term::raw`].
    pub raw: bool,
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
    /// Generated SQL of `ast`; identical aggregates over the same datasets share one term.
    pub key: String,
    /// A plain SQL aggregate beside `AGG()` (relational_semantics §7.2): evaluated
    /// over the query's joined rows as written, repeats and NULL-extended rows
    /// included, so it is always inline and never de-duplicated or filtered.
    pub raw: bool,
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
        let ast = portable::parse(&portable::spell_types(text))
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

/// The limit on an identifier's length (expression language, "Namespacing").
const MAX_IDENTIFIER: usize = 128;

/// The name of a hidden field that reads one source column.
const SOURCE_COLUMN: &str = "__source_";

pub(crate) const TERM: &str = "__ossiec_t";

pub(crate) const DIM: &str = "__ossiec_d";
const WIN: &str = "__ossiec_w";
const HEAD: &str = "__ossiec_head";

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
}
