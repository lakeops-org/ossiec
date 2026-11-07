//! AST plumbing on top of polyglot-sql.
//!
//! Query skeletons are written as SQL templates that contain only keywords,
//! generated aliases and placeholders (`__ossiec_N`, `__ossiec_aN`). The
//! template is parsed once, then every placeholder is replaced by an AST
//! node. Model and request content (expressions, table names, literals,
//! output names) is never spliced into SQL text.

use polyglot_sql::dialects::{Dialect, TranspileOptions};
use polyglot_sql::expressions::Identifier;
use polyglot_sql::generator::NormalizeFunctions;
use polyglot_sql::{parse_one, DialectType, Expression};

use crate::{Error, Result};

const PH: &str = "__ossiec_";

/// A quoted identifier for a name that may be written in its own quotes in the
/// model. polyglot can't read back the tripled quotes it would otherwise generate.
pub(crate) fn quoted(name: &str) -> Identifier {
    Identifier::quoted(crate::plan::unquoted(name))
}

/// Rewrite every node, children first. polyglot's own `transform` is selective:
/// it skips the arguments of many typed functions (`YEAR(d)`, `CONTAINS(s, x)`,
/// `FIRST_VALUE(SUM(v))`), so a field or aggregate inside one would go unseen.
/// `None` replaces the node with NULL.
pub(crate) fn transform<F>(e: Expression, f: &F) -> polyglot_sql::Result<Expression>
where
    F: Fn(Expression) -> polyglot_sql::Result<Option<Expression>>,
{
    polyglot_sql::transform_all(e, &|e| {
        Ok(f(e)?.unwrap_or(Expression::Null(polyglot_sql::expressions::Null)))
    })
}

/// A SQL template with expression and output-alias placeholders.
#[derive(Default)]
pub(crate) struct Tpl {
    sql: String,
    args: Vec<Expression>,
    aliases: Vec<String>,
}

impl Tpl {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push(&mut self, s: &str) -> &mut Self {
        self.sql.push_str(s);
        self
    }

    /// Placeholder for an expression, table or derived table.
    pub(crate) fn e(&mut self, e: Expression) -> String {
        self.args.push(e);
        format!("{PH}{}", self.args.len() - 1)
    }

    /// Placeholder for an output name, rendered as a quoted identifier.
    pub(crate) fn alias(&mut self, name: &str) -> String {
        self.aliases.push(name.to_string());
        format!("{PH}a{}", self.aliases.len() - 1)
    }

    pub(crate) fn build(self) -> Result<Expression> {
        let ast = parse_one(&self.sql, DialectType::Generic)
            .map_err(|e| Error::Internal(format!("template `{}`: {e}", self.sql)))?;
        let Tpl { args, aliases, .. } = self;
        transform(ast, &|e| {
            Ok(Some(match e {
                Expression::Column(mut c) if c.table.is_none() => match slot(&c.name.name) {
                    Some(Slot::Arg(i)) => args[i].clone(),
                    Some(Slot::Alias(i)) => {
                        c.name = quoted(&aliases[i]);
                        Expression::Column(c)
                    }
                    None => Expression::Column(c),
                },
                Expression::Table(t) if t.schema.is_none() => match slot(&t.name.name) {
                    Some(Slot::Arg(i)) => args[i].clone(),
                    _ => Expression::Table(t),
                },
                Expression::Alias(mut a) => {
                    if let Some(Slot::Alias(i)) = slot(&a.alias.name) {
                        a.alias = quoted(&aliases[i]);
                    }
                    Expression::Alias(a)
                }
                other => other,
            }))
        })
        .map_err(|e| Error::Internal(e.to_string()))
    }
}

enum Slot {
    Arg(usize),
    Alias(usize),
}

fn slot(name: &str) -> Option<Slot> {
    let rest = name.strip_prefix(PH)?;
    match rest.strip_prefix('a') {
        Some(n) => n.parse().ok().map(Slot::Alias),
        None => rest.parse().ok().map(Slot::Arg),
    }
}

/// Parse a standalone SQL fragment.
pub(crate) fn parse(sql: &str, dialect: DialectType) -> std::result::Result<Expression, String> {
    parse_one(sql, dialect).map_err(|e| e.to_string())
}

/// `left UNION ALL right UNION ALL ...`, left-deep.
pub(crate) fn union_all(parts: Vec<Expression>) -> Result<Expression> {
    let mut parts = parts.into_iter();
    let mut acc = parts.next().expect("union_all needs at least one part");
    for part in parts {
        let mut u = parse_one("SELECT 1 UNION ALL SELECT 2", DialectType::Generic)
            .map_err(|e| Error::Internal(e.to_string()))?;
        if let Expression::Union(b) = &mut u {
            b.left = acc;
            b.right = part;
        }
        acc = u;
    }
    Ok(acc)
}

/// `(query) AS alias`.
pub(crate) fn derived(query: Expression, alias: Identifier) -> Expression {
    let mut e = polyglot_sql::builder::subquery_expr(query, "_").into_inner();
    if let Expression::Subquery(s) = &mut e {
        s.alias = Some(alias);
    }
    e
}

/// A column named `name`, quoted: an output column of the compiled query.
pub(crate) fn quoted_col(name: &str) -> Expression {
    let mut e = col("x");
    if let Expression::Column(c) = &mut e {
        c.name = quoted(name);
    }
    e
}

/// A column reference built from generated, always-safe names (`base.t0`, `__ossiec_t3`).
pub(crate) fn col(name: &str) -> Expression {
    parse_one(name, DialectType::Generic).expect("generated column names always parse")
}

/// Generate `ast` in its source dialect, then transpile to `target`.
pub(crate) fn render(
    ast: &Expression,
    source: DialectType,
    target: DialectType,
    pretty: bool,
) -> Result<String> {
    // Keep function names as written: engine-specific functions from dialect
    // rules (ClickHouse `lagInFrame`) are case-sensitive, and the generic
    // generator would upper-case them.
    let sql = Dialect::get(source)
        .generate_with_overrides(ast, |c| c.normalize_functions = NormalizeFunctions::None)
        .map_err(|e| Error::Internal(e.to_string()))?;
    let mut opts = TranspileOptions::default();
    opts.pretty = pretty;
    let out = Dialect::get(source)
        .transpile_with(&sql, target, opts)
        .map_err(|e| Error::Unsupported(format!("cannot render SQL for {target:?}: {e}")))?;
    Ok(out.join(";\n"))
}

/// `table.name`, both quoted: a column of a derived table the compiler named itself.
pub(crate) fn qualified_quoted_col(table: &str, name: &str) -> Expression {
    let mut e = quoted_col(name);
    if let Expression::Column(c) = &mut e {
        c.table = Some(quoted(table));
    }
    e
}

/// A table reference named `placeholder` under `alias`: stands in for a derived
/// table until [`substitute_relation`] swaps in the compiled inner query.
pub(crate) fn placeholder_relation(placeholder: &str, alias: &str) -> Expression {
    let mut t = polyglot_sql::expressions::TableRef::new(placeholder);
    t.alias = Some(Identifier::quoted(alias));
    Expression::Table(Box::new(t))
}

/// Replace every placeholder relation `placeholder` in `ast` by `(inner) AS alias`.
/// The alias is the one the placeholder was given.
pub(crate) fn substitute_relation(
    ast: Expression,
    placeholder: &str,
    inner: &Expression,
) -> Result<Expression> {
    transform(ast, &|e| {
        Ok(Some(match e {
            Expression::Table(t) if t.schema.is_none() && t.name.name == placeholder => {
                let alias = t.alias.clone().unwrap_or_else(|| quoted(placeholder));
                derived(inner.clone(), alias)
            }
            other => other,
        }))
    })
    .map_err(|e| Error::Internal(e.to_string()))
}

/// `a IS NOT DISTINCT FROM b` where `native`, else the portable
/// `(a = b OR (a IS NULL AND b IS NULL))`.
pub(crate) fn null_safe_eq(a: Expression, b: Expression, native: bool) -> Result<Expression> {
    let mut t = Tpl::new();
    let (x, y) = (t.e(a), t.e(b));
    t.push(&if native {
        format!("{x} IS NOT DISTINCT FROM {y}")
    } else {
        format!("({x} = {y} OR ({x} IS NULL AND {y} IS NULL))")
    });
    t.build()
}
