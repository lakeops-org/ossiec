//! Metrics into terms: each aggregate a metric reads, the datasets it reads, and
//! the outer expression over them. Ephemeral metrics from Layer 2 too.

use std::cell::RefCell;
use std::collections::BTreeSet;

use polyglot_sql::expressions::Column;
use polyglot_sql::{
    contains_window_function, get_aggregate_functions, get_columns, is_aggregate, DialectType,
    Expression,
};

use crate::sql::{self, aggregate_filter, clear_aggregate_filter, transform};
use crate::wide_table::ErrorCode;
use crate::{Error, Result};

use super::{
    is_hidden, lookup, true_division, Ctx, Flavor, MetricInfo, MetricRejection, ResolvedMetric,
    SemanticGraph, Term, Variant, DIM, HEAD, TERM, WIN,
};

/// The SQL type of the one field an aggregate reads, when the model declares it.
pub(super) fn declared_type(datatypes: &BTreeSet<Option<String>>) -> Option<&'static str> {
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

/// Type family of an aggregate's result: numeric for counting and arithmetic
/// aggregates; for MIN/MAX/ANY_VALUE, the type of the one field they read.
pub(super) fn null_type(
    e: &Expression,
    datatypes: &BTreeSet<Option<String>>,
) -> Option<&'static str> {
    match e {
        Expression::Min(_) | Expression::Max(_) | Expression::AnyValue(_) => {
            declared_type(datatypes)
        }
        Expression::AggregateFunction(_) => None,
        // `PERCENTILE_DISC` returns a value of what it sorts, like `MIN`.
        // `PERCENTILE_CONT` interpolates, so it's numeric.
        Expression::WithinGroup(w) => match &w.this {
            Expression::AggregateFunction(a) if a.name.eq_ignore_ascii_case("PERCENTILE_DISC") => {
                declared_type(datatypes)
            }
            Expression::AggregateFunction(a) if a.name.eq_ignore_ascii_case("PERCENTILE_CONT") => {
                Some("NUMERIC")
            }
            _ => None,
        },
        Expression::Filter(f) => null_type(&f.this, datatypes),
        _ => Some("NUMERIC"),
    }
}

pub(super) fn dup_insensitive(e: &Expression) -> bool {
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

pub(super) fn is_term_placeholder(c: &Column) -> bool {
    c.table.is_none() && c.name.name.starts_with(TERM)
}

/// `PERCENTILE_CONT(p) WITHIN GROUP (ORDER BY x) FILTER (WHERE q)` as one aggregate
/// over `p, x` with a filter, the way §5.2's checks read every other aggregate.
pub(super) fn flat_aggregates(ast: Expression) -> Expression {
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
pub(super) fn term_index(e: &Expression) -> Option<usize> {
    match e {
        Expression::Column(c) if is_term_placeholder(c) => c.name.name[TERM.len()..].parse().ok(),
        _ => None,
    }
}

pub(super) fn is_placeholder(c: &Column) -> bool {
    c.table.is_none() && c.name.name.starts_with("__ossiec_")
}

/// Apply `f` to the arguments of an aggregate, keeping the aggregate itself.
/// `None` for aggregates ossiec doesn't know how to open.
pub(super) fn map_agg_args(
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
    /// A dataset-scoped metric that exists only for this compile (a query-local
    /// `SUM|COUNT|AVG|MIN|MAX(...) TO MEASURE`). `name` is `dataset.metric`.
    /// The bound model is left unchanged; callers install on a clone.
    pub(crate) fn install_ephemeral_metric(
        &mut self,
        name: &str,
        owner: usize,
        ast: Expression,
        raw: bool,
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
            raw,
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
    pub(super) fn check_filter_datasets(
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

    pub(super) fn column_datasets(
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

    pub(super) fn resolve_column(
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

    pub(super) fn resolve_metric_unchecked(
        &self,
        m: usize,
        ctx: &mut Ctx,
    ) -> Result<ResolvedMetric> {
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
            // The type of what the aggregate reads, not of what its FILTER reads.
            let body = clear_aggregate_filter(raw.clone());
            let datatypes: BTreeSet<Option<String>> = get_columns(&body)
                .iter()
                .filter_map(|c| match c {
                    Expression::Column(c) => {
                        let mut ctx = ctx_cell.borrow_mut();
                        self.resolve_column(c, info.owner, &mut ctx).ok()
                    }
                    _ => None,
                })
                .map(|(ds, f)| self.datasets[ds].fields[f].datatype.clone())
                .collect();
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
                raw: info.raw,
            });
        }
        Ok(ResolvedMetric {
            outer,
            terms,
            dims,
            windowed,
        })
    }

    pub(super) fn required_dims(&self, metric: &str) -> Vec<String> {
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
}
