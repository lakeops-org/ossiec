//! `FILTER (WHERE ...)` for engines without it: rewritten to `CASE WHEN` inside
//! the aggregate, statistical aggregates included.

use polyglot_sql::Expression;

use crate::sql::{self, transform, Tpl};
use crate::{Error, Result};

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
