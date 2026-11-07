//! Ossie's portable expression language (`expression_language.md`): what it
//! refuses, and what it spells differently from polyglot's AST.

use polyglot_sql::expressions::Var;
use polyglot_sql::{is_query, is_subquery, Expression, ExpressionWalk};

use crate::sql::transform;

/// A standard name for a type SQL has no word for. `TIMESTAMP_NTZ` is a wall-clock
/// timestamp, which is what `TIMESTAMP` means in standard SQL. polyglot can't parse
/// the typed literal, so this runs on the text, outside strings and quoted names.
pub(crate) fn spell_types(text: &str) -> String {
    const NTZ: &str = "TIMESTAMP_NTZ";
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut out = String::with_capacity(text.len());
    let mut quote = None;
    let mut rest = text;
    let mut after_word = false;
    while let Some(c) = rest.chars().next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if !after_word
                && rest.len() >= NTZ.len()
                && rest.is_char_boundary(NTZ.len())
                && rest[..NTZ.len()].eq_ignore_ascii_case(NTZ)
                && !rest[NTZ.len()..].starts_with(is_word) =>
            {
                out.push_str("TIMESTAMP");
                rest = &rest[NTZ.len()..];
                after_word = true;
                continue;
            }
            None => {}
        }
        after_word = is_word(c);
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// What the language doesn't allow in an expression, if `ast` has any of it.
pub(crate) fn refusal(ast: &Expression) -> Option<&'static str> {
    if ast.contains(|e| is_query(e) || is_subquery(e)) {
        return Some("subqueries aren't part of the expression language");
    }
    let is_agg = |e: &Expression| match e {
        Expression::Function(f) => f.name.eq_ignore_ascii_case("AGG"),
        Expression::AggregateFunction(f) => f.name.eq_ignore_ascii_case("AGG"),
        _ => false,
    };
    if ast.contains(is_agg) {
        return Some("AGG is reserved for evaluating a measure in a query, and no expression may call it yet");
    }
    None
}

/// Name the expression as polyglot and every engine expect it.
pub(crate) fn normalize(ast: Expression) -> Expression {
    transform(ast, &|e| {
        Ok(Some(match e {
            // The language's `STDDEV` and `VARIANCE` are the sample forms. On
            // MySQL and StarRocks the same names mean the population ones.
            // Evidence: `STDDEV(v)`, `VARIANCE(v)` in `expression_language.rs`.
            // `name` is the spelling the generator would otherwise keep.
            Expression::Stddev(mut f) => {
                f.name = None;
                Expression::StddevSamp(f)
            }
            Expression::Variance(mut f) => {
                f.name = None;
                Expression::VarSamp(f)
            }
            // `DATEADD(day, ...)`: the part is a keyword, not a column.
            Expression::Function(mut f)
                if f.name.eq_ignore_ascii_case("DATEADD")
                    || f.name.eq_ignore_ascii_case("DATEDIFF") =>
            {
                if let Some(Expression::Column(c)) = f.args.first() {
                    if c.table.is_none() && !c.name.quoted {
                        f.args[0] = Expression::Var(Box::new(Var {
                            this: c.name.name.to_ascii_uppercase(),
                        }));
                    }
                }
                Expression::Function(f)
            }
            other => other,
        }))
    })
    .expect("naming nodes cannot fail")
}
