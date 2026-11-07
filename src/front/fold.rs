//! `AGG()` over a sole derived table (relational_semantics §7.7).
//!
//! ```sql
//! SELECT region, AGG(order_count)
//! FROM (SELECT region, order_count FROM orders WHERE status = 'completed') t
//! GROUP BY region
//! ```
//!
//! `order_count` stays a measure through the subquery, and the outer `AGG` evaluates
//! it over the source rows that survive the inner query: the derived table is only
//! a filter and a projection. It is therefore **folded** into the outer stage,
//! syntactically, before anything is planned:
//!
//! ```sql
//! SELECT region, AGG(order_count) FROM orders WHERE status = 'completed' GROUP BY region
//! ```
//!
//! The folded query keeps the inner `FROM` / `JOIN` / `WHERE` and takes the outer
//! select list, `WHERE` (and-ed on), `GROUP BY`, `ORDER BY` and `LIMIT`, with every
//! reference to a derived column replaced by the inner expression it names. The
//! result is an ordinary `AGG` stage ([`Compiler::agg_stage`]). An inner `GROUP BY`
//! is dropped (it only fixes a grain; the measure is re-evaluated at the outer one).
//!
//! Inners that would change which rows feed the measure (`HAVING`, `DISTINCT`,
//! `ORDER BY` / `LIMIT`, `WITH`, a derived `FROM`) are refused. The derived-table
//! join form (`FROM orders o JOIN (...) t ON ...`) does not fold; see
//! [`super::nested`].

use polyglot_sql::expressions::{
    Alias, BinaryOp, Column, GroupBy, Identifier, OrderBy, Ordered, Select, Where,
};
use polyglot_sql::Expression;

use super::nested::{Compiler, Node};
use super::to_measure::marker_index;
use super::{
    as_column, as_derived, bare_measure, classify_to_measure, is_agg_name, parse_from_clause,
    strip_alias, unsupported, unwrap_paren,
};
use crate::{Error, Result};

/// Does a select item contain `AGG(...)` / `MEASURE(...)` (in an arithmetic composition)?
pub(super) fn has_agg(e: &Expression) -> bool {
    let (e, _) = strip_alias(e);
    match unwrap_paren(e) {
        Expression::Function(f) if is_agg_name(&f.name) => true,
        Expression::AggregateFunction(f) if is_agg_name(&f.name) => true,
        Expression::Add(b) | Expression::Sub(b) | Expression::Mul(b) | Expression::Div(b) => {
            has_agg(&b.left) || has_agg(&b.right)
        }
        Expression::Function(f) if f.name.eq_ignore_ascii_case("NULLIF") => {
            f.args.iter().any(has_agg)
        }
        _ => false,
    }
}

/// What a column of the derived table stands for.
enum Col {
    /// A bare inner column (a dimension, or a measure passed through).
    Inner(Expression),
    /// A `TO MEASURE` item: the expression an `AGG()` of it expands to.
    Deferred(Expression),
    /// An expression of the inner select list: a value, not addressable here.
    Evaluated,
}

struct FoldMap {
    alias: String,
    cols: Vec<(String, Col)>,
}

impl FoldMap {
    fn missing(&self, name: &str) -> Error {
        Error::Request(format!(
            "derived table `{}` has no column `{name}`; it has: {}",
            self.alias,
            self.cols
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    /// The column a reference names on the derived table. Foreign relation
    /// qualifiers and names the derived table does not expose are rejected: after
    /// fold they would otherwise resolve against the inner FROM and leak.
    fn lookup(&self, c: &Column) -> Result<&Col> {
        let name = &c.name.name;
        if let Some(t) = &c.table {
            if !t.name.eq_ignore_ascii_case(&self.alias) {
                return Err(Error::Request(format!(
                    "derived table `{}` does not expose `{}.{}`; qualify columns with `{}` or use a name from its select list",
                    self.alias, t.name, name, self.alias
                )));
            }
        }
        self.cols
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, c)| c)
            .ok_or_else(|| self.missing(name))
    }

    fn evaluated(&self, name: &str) -> Error {
        unsupported(format!(
            "Layer 2 AGG() applies to measures; `{}.{name}` of a derived table is already evaluated: use it as a plain column (outer AVG/SUM/...) or pass the measure through unevaluated",
            self.alias
        ))
    }

    /// Rewrite one expression of the outer query. `skip` names unqualified
    /// columns that are select aliases (`ORDER BY n`) and stay as they are.
    fn expr(&self, e: &Expression, skip: &[String]) -> Result<Expression> {
        let rec = |x: &Expression| self.expr(x, skip);
        let bin = |b: &BinaryOp| -> Result<BinaryOp> {
            let mut n = b.clone();
            n.left = rec(&b.left)?;
            n.right = rec(&b.right)?;
            Ok(n)
        };
        Ok(match e {
            Expression::Column(c) => {
                if c.table.is_none() && skip.iter().any(|s| s.eq_ignore_ascii_case(&c.name.name)) {
                    return Ok(e.clone());
                }
                match self.lookup(c)? {
                    Col::Inner(x) => x.clone(),
                    Col::Deferred(_) => {
                        return Err(unsupported(format!(
                            "Layer 2 `{}.{}` is a TO MEASURE column: only AGG({0}.{1}) can read it",
                            self.alias, c.name.name
                        )))
                    }
                    Col::Evaluated => {
                        return Err(unsupported(format!(
                            "Layer 2 `{}.{}` is an evaluated measure; an outer query with AGG() cannot read it as a column",
                            self.alias, c.name.name
                        )))
                    }
                }
            }
            Expression::Function(f) if is_agg_name(&f.name) && f.args.len() == 1 => {
                match self.agg_arg(&f.args[0])? {
                    Agg::Keep(a) => {
                        let mut n = (**f).clone();
                        n.args[0] = a;
                        Expression::Function(Box::new(n))
                    }
                    Agg::Expand(x) => x,
                }
            }
            Expression::AggregateFunction(f) if is_agg_name(&f.name) && f.args.len() == 1 => {
                // Reject FILTER before expanding a TO MEASURE formula (which would
                // drop the clause and skip take_agg's check).
                if f.filter.is_some() {
                    return Err(unsupported(
                        "FILTER on AGG() itself is not defined; put filters in WHERE or in the measure",
                    ));
                }
                match self.agg_arg(&f.args[0])? {
                    Agg::Keep(a) => {
                        let mut n = (**f).clone();
                        n.args[0] = a;
                        Expression::AggregateFunction(Box::new(n))
                    }
                    Agg::Expand(x) => x,
                }
            }
            Expression::Function(f) if f.name.eq_ignore_ascii_case("NULLIF") => {
                let mut n = (**f).clone();
                n.args = f.args.iter().map(rec).collect::<Result<_>>()?;
                Expression::Function(Box::new(n))
            }
            Expression::Paren(p) => {
                let mut n = (**p).clone();
                n.this = rec(&p.this)?;
                Expression::Paren(Box::new(n))
            }
            Expression::Alias(a) => {
                let mut n = (**a).clone();
                n.this = rec(&a.this)?;
                Expression::Alias(Box::new(n))
            }
            Expression::Add(b) => Expression::Add(Box::new(bin(b)?)),
            Expression::Sub(b) => Expression::Sub(Box::new(bin(b)?)),
            Expression::Mul(b) => Expression::Mul(Box::new(bin(b)?)),
            Expression::Div(b) => Expression::Div(Box::new(bin(b)?)),
            Expression::And(b) => Expression::And(Box::new(bin(b)?)),
            Expression::Eq(b) => Expression::Eq(Box::new(bin(b)?)),
            Expression::Neq(b) => Expression::Neq(Box::new(bin(b)?)),
            Expression::Lt(b) => Expression::Lt(Box::new(bin(b)?)),
            Expression::Lte(b) => Expression::Lte(Box::new(bin(b)?)),
            Expression::Gt(b) => Expression::Gt(Box::new(bin(b)?)),
            Expression::Gte(b) => Expression::Gte(Box::new(bin(b)?)),
            Expression::IsNull(i) => {
                let mut n = (**i).clone();
                n.this = rec(&i.this)?;
                Expression::IsNull(Box::new(n))
            }
            other => other.clone(),
        })
    }

    /// The argument of an `AGG(...)` over this derived table.
    fn agg_arg(&self, arg: &Expression) -> Result<Agg> {
        let Some(c) = as_column(arg) else {
            return Ok(Agg::Keep(arg.clone()));
        };
        Ok(match self.lookup(c)? {
            Col::Inner(x) => Agg::Keep(x.clone()),
            Col::Deferred(x) => Agg::Expand(Expression::Paren(Box::new(
                polyglot_sql::expressions::Paren {
                    this: x.clone(),
                    trailing_comments: vec![],
                },
            ))),
            Col::Evaluated => return Err(self.evaluated(&c.name.name)),
        })
    }

    /// Keep the consumer-visible select label when rewriting replaces a bare
    /// column (`s`) with its inner expression (`status`). Explicit `AS` aliases
    /// are already preserved by [`Self::expr`]'s Alias arm.
    fn with_label(&self, original: &Expression, rewritten: Expression) -> Expression {
        let (inner, alias) = strip_alias(original);
        if alias.is_some() {
            // `expr` already rewrote inside the Alias node.
            return rewritten;
        }
        let Some(c) = as_column(inner) else {
            return rewritten;
        };
        let label = &c.name.name;
        if as_column(&rewritten)
            .is_some_and(|r| r.table.is_none() && r.name.name.eq_ignore_ascii_case(label))
        {
            return rewritten;
        }
        Expression::Alias(Box::new(Alias::new(
            rewritten,
            Identifier::new(label.clone()),
        )))
    }
}

enum Agg {
    /// `AGG(<this>)`.
    Keep(Expression),
    /// Replaces the whole `AGG(...)` call (a `TO MEASURE` formula).
    Expand(Expression),
}

impl Compiler<'_> {
    /// Fold the sole derived table of `sel` into it and compile the result as an
    /// `AGG` stage.
    pub(super) fn fold(
        &mut self,
        sel: &Select,
        depth: usize,
        derived_mode: bool,
        outer: &[String],
    ) -> Result<Node> {
        if sel.distinct_on.is_some() || sel.qualify.is_some() || sel.offset.is_some() {
            return Err(unsupported(
                "Layer 2 DISTINCT ON / QUALIFY / OFFSET are not implemented yet",
            ));
        }
        let from = sel.from.as_ref().expect("classified as a fold");
        let (inner, alias) = as_derived(&from.expressions[0])?.expect("classified as a fold");

        let why = |what: &str| {
            unsupported(format!(
                "Layer 2 AGG() over derived table `{alias}` (relational_semantics §7.7) folds a plain filter + projection of model datasets; the derived table has {what}. Join the derived table to a dataset (FROM <dataset> JOIN (...) {alias} ON ...) instead"
            ))
        };
        if inner.distinct || inner.having.is_some() || inner.qualify.is_some() {
            return Err(why("DISTINCT / HAVING / QUALIFY"));
        }
        if inner.order_by.is_some() || inner.limit.is_some() || inner.offset.is_some() {
            return Err(why("ORDER BY / LIMIT / OFFSET"));
        }
        let inner_from = inner.from.as_ref().ok_or_else(|| why("no FROM"))?;
        for e in &inner_from.expressions {
            if as_derived(e)?.is_some() {
                return Err(why("a derived table as its FROM"));
            }
        }

        // What each derived column stands for. A base aggregate becomes a
        // pass-through of an ephemeral metric; an AGG() composition stays deferred
        // and expands in place of the outer AGG().
        let scope = parse_from_clause(&self.graph, inner.from.as_ref(), &inner.joins, outer)?.scope;
        let mut cols: Vec<(String, Col)> = vec![];
        let mut pending = vec![];
        for item in &inner.expressions {
            let (e, a) = strip_alias(item);
            let (name, col) = match (as_column(e), a) {
                (Some(c), a) => {
                    let name = a.unwrap_or_else(|| c.name.name.clone());
                    match marker_index(c) {
                        Some(n) => {
                            let expr = self
                                .to_measures
                                .get(n)
                                .cloned()
                                .ok_or_else(|| Error::Internal("TO MEASURE marker".into()))?;
                            if let Some(ep) =
                                classify_to_measure(&self.graph, &scope, &expr, &mut self.base_seq)?
                            {
                                let col =
                                    Col::Inner(Expression::qualified_column(&ep.dataset, &ep.bare));
                                pending.push(ep);
                                (name, col)
                            } else {
                                (name, Col::Deferred(expr))
                            }
                        }
                        None => (name, Col::Inner(e.clone())),
                    }
                }
                (None, Some(a)) => (a, Col::Evaluated),
                (None, None) => continue,
            };
            if cols.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name)) {
                return Err(Error::Request(format!(
                    "Layer 2 derived table has two output columns named `{name}`; give one an alias"
                )));
            }
            cols.push((name, col));
        }
        let map = FoldMap { alias, cols };

        // Inner GROUP BY is dropped (grain is re-evaluated), but grouping by a
        // measure must still be refused.
        if let Some(gb) = &inner.group_by {
            let inner_scope =
                parse_from_clause(&self.graph, inner.from.as_ref(), &inner.joins, &[])?.scope;
            for e in &gb.expressions {
                let col = as_column(e)
                    .ok_or_else(|| unsupported("Layer 2 GROUP BY must name dimension columns"))?;
                // Prefer the derived select label, then the inner expression.
                let measure =
                    map.cols
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case(&col.name.name))
                        .and_then(|(_, c)| match c {
                            Col::Inner(x) => as_column(x)
                                .and_then(|c| bare_measure(&self.graph, &inner_scope, c)),
                            Col::Deferred(_) | Col::Evaluated => Some(0),
                        })
                        .or_else(|| bare_measure(&self.graph, &inner_scope, col));
                if measure.is_some() {
                    return Err(Error::Request(format!(
                        "Layer 2 GROUP BY cannot name measure `{}`; group by dimensions only",
                        col.name.name
                    )));
                }
            }
        }

        // Aliases the outer select list defines (`ORDER BY n`).
        let select_aliases: Vec<String> = sel
            .expressions
            .iter()
            .filter_map(|e| strip_alias(e).1)
            .collect();

        let mut folded = inner.clone();
        folded.expressions = sel
            .expressions
            .iter()
            .map(|e| {
                let rewritten = map.expr(e, &[])?;
                Ok(map.with_label(e, rewritten))
            })
            .collect::<Result<_>>()?;
        folded.where_clause = match (&inner.where_clause, &sel.where_clause) {
            (None, None) => None,
            (Some(w), None) => Some(w.clone()),
            (None, Some(w)) => Some(Where {
                this: map.expr(&w.this, &[])?,
            }),
            (Some(a), Some(b)) => Some(Where {
                this: Expression::And(Box::new(BinaryOp::new(
                    a.this.clone(),
                    map.expr(&b.this, &[])?,
                ))),
            }),
        };
        folded.group_by = match &sel.group_by {
            None => None,
            Some(g) => Some(GroupBy {
                expressions: g
                    .expressions
                    .iter()
                    .map(|e| map.expr(e, &[]))
                    .collect::<Result<_>>()?,
                ..g.clone()
            }),
        };
        folded.order_by = match &sel.order_by {
            None => None,
            Some(o) => Some(OrderBy {
                expressions: o
                    .expressions
                    .iter()
                    .map(|k| {
                        Ok(Ordered {
                            this: map.expr(&k.this, &select_aliases)?,
                            ..k.clone()
                        })
                    })
                    .collect::<Result<_>>()?,
                ..o.clone()
            }),
        };
        folded.having = match &sel.having {
            None => None,
            Some(h) => {
                let mut having = h.clone();
                having.this = map.expr(&h.this, &[])?;
                Some(having)
            }
        };
        folded.limit = sel.limit.clone();
        folded.offset = None;
        for ep in pending {
            self.graph
                .install_ephemeral_metric(&ep.name, ep.owner, ep.body)?;
        }
        self.agg_stage(&folded, depth, derived_mode, outer)
    }
}
