//! Ossie's functions, engine by engine (`expression_language.md`).
//!
//! polyglot translates SQL between dialects, and ossiec keeps it for that. But the
//! functions below are defined by Ossie, and reading them as polyglot's Generic
//! dialect leaves their meaning to chance: many pass through to engines that don't
//! have them, and some are translated into a different answer. So each has an
//! implementation here, before polyglot sees the query:
//!
//! - [`standard`]: the form in core SQL, for every engine.
//! - [`on_engine`]: where one engine needs its own form, or has no faithful one.
//!
//! An engine with no faithful form refuses, so a query never gets a wrong answer.
//! `__ossiec_exact_NAME` is a function name polyglot would otherwise rewrite (it turns
//! `CHAR_LENGTH` into `LENGTH`, which counts bytes on MySQL); `dialect::finish` removes
//! the prefix after polyglot is done.
//! Evidence for every rule is in `tests/expression_language.rs`.
//!
//! Only a statement read entirely in the portable language comes through here: a
//! vendor expression keeps its engine's meaning.

use std::cell::RefCell;

use polyglot_sql::expressions::{DataType, DateTimeField, Literal};
use polyglot_sql::{DialectType, Expression};

use crate::sql::{transform, Tpl};
use crate::{Error, Result};

enum Rule {
    /// SQL with `{0}`, `{1}` for the arguments.
    Sql(&'static str),
    /// The engine has no form that means the same.
    Refuse,
}
use Rule::{Refuse, Sql};

/// One call: its key, arguments and `FILTER`, if any.
struct Call {
    key: String,
    args: Vec<Expression>,
    filter: Option<Expression>,
}

fn call(key: impl Into<String>, args: Vec<Expression>) -> Option<Call> {
    Some(Call {
        key: key.into(),
        args,
        filter: None,
    })
}

fn text(e: &Expression) -> Option<&str> {
    match e {
        Expression::Literal(l) => match l.as_ref() {
            Literal::String(s) => Some(s),
            _ => None,
        },
        _ => None,
    }
}

fn field_name(f: &DateTimeField) -> Option<String> {
    Some(match f {
        DateTimeField::Year => "YEAR".into(),
        DateTimeField::Quarter => "QUARTER".into(),
        DateTimeField::Month => "MONTH".into(),
        DateTimeField::Week => "WEEK".into(),
        DateTimeField::Day => "DAY".into(),
        DateTimeField::DayOfWeek => "DAYOFWEEK".into(),
        DateTimeField::DayOfYear => "DAYOFYEAR".into(),
        DateTimeField::Hour => "HOUR".into(),
        DateTimeField::Minute => "MINUTE".into(),
        DateTimeField::Second => "SECOND".into(),
        DateTimeField::Millisecond => "MILLISECOND".into(),
        DateTimeField::Custom(c) => c.to_ascii_uppercase(),
        _ => return None,
    })
}

/// A part Ossie names as a keyword, as in `DATEADD(day, ...)`.
fn keyword_part(e: &Expression) -> Option<String> {
    match e {
        Expression::Var(v) => Some(v.this.to_ascii_uppercase()),
        _ => None,
    }
}

/// Which Ossie function this node calls. polyglot gives some functions their own
/// node and leaves others as a plain call, so both are read here.
fn ossie_call(e: &Expression) -> Option<Call> {
    let extract = |part: &str, x: &Expression| call(format!("EXTRACT:{part}"), vec![x.clone()]);
    match e {
        Expression::Year(f) => extract("YEAR", &f.this),
        Expression::Quarter(f) => extract("QUARTER", &f.this),
        Expression::Month(f) => extract("MONTH", &f.this),
        Expression::Day(f) => extract("DAY", &f.this),
        Expression::DayOfYear(f) => extract("DAYOFYEAR", &f.this),
        Expression::Hour(f) => extract("HOUR", &f.this),
        Expression::Minute(f) => extract("MINUTE", &f.this),
        Expression::Second(f) => extract("SECOND", &f.this),
        Expression::Extract(f) => extract(&field_name(&f.field)?, &f.this),
        Expression::Length(f) => call("LENGTH", vec![f.this.clone()]),
        Expression::Upper(f) => call("UPPER", vec![f.this.clone()]),
        Expression::Lower(f) => call("LOWER", vec![f.this.clone()]),
        Expression::StrPosition(f) if f.position.is_none() && f.occurrence.is_none() => call(
            "POSITION",
            vec![(**f.substr.as_ref()?).clone(), (*f.this).clone()],
        ),
        Expression::Substring(f) => call(
            "SUBSTRING",
            vec![f.this.clone(), f.start.clone(), f.length.clone()?],
        ),
        Expression::Contains(f) => call("CONTAINS", vec![f.this.clone(), f.expression.clone()]),
        Expression::StartsWith(f) => call("STARTSWITH", vec![f.this.clone(), f.expression.clone()]),
        Expression::EndsWith(f) => call("ENDSWITH", vec![f.this.clone(), f.expression.clone()]),
        Expression::IfFunc(f) => call(
            "IF",
            vec![
                f.condition.clone(),
                f.true_value.clone(),
                f.false_value.clone()?,
            ],
        ),
        Expression::TryCast(_) => call("TRY_CAST", vec![e.clone()]),
        Expression::CumeDist(_) => call("CUME_DIST", vec![]),
        Expression::Concat(op) => call("CONCAT_OP", vec![op.left.clone(), op.right.clone()]),
        Expression::NullSafeEq(op) => call("NULL_SAFE_EQ", vec![op.left.clone(), op.right.clone()]),
        Expression::NullSafeNeq(op) => {
            call("NULL_SAFE_NEQ", vec![op.left.clone(), op.right.clone()])
        }
        Expression::Cast(c) if matches!(c.to, DataType::Float { .. }) => {
            call("CAST_FLOAT", vec![c.this.clone()])
        }
        Expression::Log(f) => call("LOG", vec![f.base.clone()?, f.this.clone()]),
        Expression::Literal(l) => match l.as_ref() {
            Literal::Time(s) => call(
                "TIME_LITERAL",
                vec![Expression::Literal(Box::new(Literal::String(s.clone())))],
            ),
            _ => None,
        },
        Expression::Median(f) if !f.distinct => Some(Call {
            key: "MEDIAN".into(),
            args: vec![f.this.clone()],
            filter: f.filter.clone(),
        }),
        Expression::ApproxCountDistinct(f) => Some(Call {
            key: "APPROX_COUNT_DISTINCT".into(),
            args: vec![f.this.clone()],
            filter: f.filter.clone(),
        }),
        Expression::AggregateFunction(f)
            if f.name.eq_ignore_ascii_case("APPROX_PERCENTILE") && f.args.len() == 2 =>
        {
            Some(Call {
                key: "APPROX_PERCENTILE".into(),
                args: f.args.clone(),
                filter: f.filter.clone(),
            })
        }
        Expression::WithinGroup(w) => within_group(w),
        Expression::Filter(f) => match &*f.this {
            Expression::WithinGroup(w) => {
                let mut c = within_group(w)?;
                c.filter = Some((*f.expression).clone());
                Some(c)
            }
            _ => None,
        },
        Expression::Function(f) => {
            let name = f.name.to_ascii_uppercase();
            let args = &f.args;
            match (name.as_str(), args.len()) {
                ("CURRENT_DATE" | "CURRENT_TIME" | "CURRENT_TIMESTAMP", 0) => call(name, vec![]),
                ("DATE_PART", 2) => {
                    let part = text(&args[0])?.to_ascii_uppercase();
                    call(format!("EXTRACT:{part}"), vec![args[1].clone()])
                }
                ("DATE_TRUNC", 2) => {
                    let part = text(&args[0])?.to_ascii_uppercase();
                    call(format!("DATE_TRUNC:{part}"), vec![args[1].clone()])
                }
                ("DATEADD", 3) => {
                    let part = keyword_part(&args[0])?;
                    call(
                        format!("DATEADD:{part}"),
                        vec![args[1].clone(), args[2].clone()],
                    )
                }
                ("DATEDIFF", 3) => {
                    let part = keyword_part(&args[0])?;
                    call(
                        format!("DATEDIFF:{part}"),
                        vec![args[1].clone(), args[2].clone()],
                    )
                }
                ("TO_DATE" | "TO_TIMESTAMP" | "TO_CHAR", n) => {
                    call(format!("{name}:{n}"), args.clone())
                }
                ("ROUND" | "TRUNC" | "TRUNCATE", 2) => {
                    call(name.replace("TRUNCATE", "TRUNC"), args.clone())
                }
                // Only the forms a rule handles. Any other form isn't Ossie's, so it
                // keeps its arguments and passes through.
                ("CONCAT", n) if n >= 2 => call(name, args.clone()),
                ("ZEROIFNULL" | "NULLIFZERO", 1)
                | (
                    "LEFT" | "RIGHT" | "CHARINDEX" | "REGEXP_LIKE" | "REGEXP_EXTRACT"
                    | "REGEXP_COUNT",
                    2,
                )
                | ("SPLIT_PART" | "REGEXP_REPLACE", 3) => call(name, args.clone()),
                _ => None,
            }
        }
        _ => None,
    }
}

fn within_group(w: &polyglot_sql::expressions::WithinGroup) -> Option<Call> {
    let Expression::AggregateFunction(f) = &w.this else {
        return None;
    };
    let name = f.name.to_ascii_uppercase();
    if !matches!(name.as_str(), "PERCENTILE_CONT" | "PERCENTILE_DISC") {
        return None;
    }
    match (f.args.as_slice(), w.order_by.as_slice()) {
        ([p], [o]) if !o.desc => call(name, vec![p.clone(), o.this.clone()]),
        _ => None,
    }
}

/// The form in core SQL, which every engine gets unless it has its own.
fn standard(key: &str) -> Option<&'static str> {
    Some(match key {
        "CURRENT_DATE" => "CURRENT_DATE",
        "CURRENT_TIME" => "CURRENT_TIME",
        "CURRENT_TIMESTAMP" => "CURRENT_TIMESTAMP",
        "ZEROIFNULL" => "COALESCE({0}, 0)",
        "NULLIFZERO" => "NULLIF({0}, 0)",
        "IF" => "CASE WHEN {0} THEN {1} ELSE {2} END",
        "CHARINDEX" => "POSITION({0} IN {1})",
        "CONTAINS" => "POSITION({1} IN {0}) > 0",
        "STARTSWITH" => "LEFT({0}, LENGTH({1})) = {1}",
        "ENDSWITH" => "RIGHT({0}, LENGTH({1})) = {1}",
        "TO_DATE:1" => "CAST({0} AS DATE)",
        "TO_TIMESTAMP:1" => "CAST({0} AS TIMESTAMP)",
        "EXTRACT:YEAR" => "EXTRACT(YEAR FROM {0})",
        "EXTRACT:QUARTER" => "EXTRACT(QUARTER FROM {0})",
        "EXTRACT:MONTH" => "EXTRACT(MONTH FROM {0})",
        "EXTRACT:WEEK" => "EXTRACT(WEEK FROM {0})",
        "EXTRACT:DAY" => "EXTRACT(DAY FROM {0})",
        "EXTRACT:DAYOFWEEK" => "EXTRACT(DOW FROM {0})",
        "EXTRACT:DAYOFYEAR" => "EXTRACT(DOY FROM {0})",
        "EXTRACT:HOUR" => "EXTRACT(HOUR FROM {0})",
        "EXTRACT:MINUTE" => "EXTRACT(MINUTE FROM {0})",
        "EXTRACT:SECOND" => "EXTRACT(SECOND FROM {0})",
        "EXTRACT:MILLISECOND" => "EXTRACT(MILLISECOND FROM {0})",
        _ => return None,
    })
}

/// `a || b || ...`, for a `CONCAT` of any length.
const CONCAT_BY_OPERATOR: &str = "__concat_by_operator";

/// Where an engine needs its own form.
fn on_engine(target: DialectType, key: &str) -> Option<Rule> {
    use DialectType::{ClickHouse, DuckDB, MySQL, PostgreSQL, StarRocks, Trino};
    Some(match (target, key) {
        // DuckDB
        (DuckDB, "CURRENT_TIMESTAMP") => Sql("CURRENT_TIMESTAMP"),
        // NULL makes the result NULL, as on the other engines here. These two would skip it.
        (DuckDB | PostgreSQL, "CONCAT") => Sql(CONCAT_BY_OPERATOR),
        (DuckDB, "REGEXP_EXTRACT") => Sql("REGEXP_EXTRACT({0}, {1}, 0)"),
        (DuckDB, "REGEXP_REPLACE") => Sql("REGEXP_REPLACE({0}, {1}, {2}, 'g')"),
        (DuckDB, "REGEXP_COUNT") => Sql("LEN(REGEXP_EXTRACT_ALL({0}, {1}))"),
        (DuckDB, "APPROX_PERCENTILE") => Sql("APPROX_QUANTILE({0}, {1})"),
        (DuckDB, "DATEADD:YEAR") => Sql("{1} + {0} * INTERVAL '1 YEAR'"),
        (DuckDB, "DATEADD:QUARTER") => Sql("{1} + 3 * {0} * INTERVAL '1 MONTH'"),
        (DuckDB, "DATEADD:MONTH") => Sql("{1} + {0} * INTERVAL '1 MONTH'"),
        (DuckDB, "DATEADD:WEEK") => Sql("{1} + {0} * INTERVAL '7 DAY'"),
        (DuckDB, "DATEADD:DAY") => Sql("{1} + {0} * INTERVAL '1 DAY'"),
        (DuckDB, "DATEADD:HOUR") => Sql("{1} + {0} * INTERVAL '1 HOUR'"),
        (DuckDB, "DATEADD:MINUTE") => Sql("{1} + {0} * INTERVAL '1 MINUTE'"),
        (DuckDB, "DATEADD:SECOND") => Sql("{1} + {0} * INTERVAL '1 SECOND'"),
        (DuckDB, "TO_CHAR:2") => Sql("STRFTIME({0}, {1})"),
        (DuckDB, "TO_DATE:2") => Sql("CAST(STRPTIME({0}, {1}) AS DATE)"),
        (DuckDB, "TO_TIMESTAMP:2") => Sql("STRPTIME({0}, {1})"),

        // PostgreSQL. Its DATEDIFF would count whole units, and DuckDB's, ClickHouse's
        // and the syntax's own origin count the boundaries crossed.
        (PostgreSQL, "ROUND") => Sql("ROUND(CAST({0} AS NUMERIC), {1})"),
        (PostgreSQL, "TRUNC") => Sql("TRUNC(CAST({0} AS NUMERIC), {1})"),
        (PostgreSQL, "TRY_CAST") => Refuse,
        (PostgreSQL, "REGEXP_EXTRACT") => Sql("SUBSTRING({0} FROM {1})"),
        (PostgreSQL, "REGEXP_REPLACE") => Sql("REGEXP_REPLACE({0}, {1}, {2}, 'g')"),
        (PostgreSQL, "MEDIAN") => Sql("PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY {0})"),
        (PostgreSQL, "APPROX_COUNT_DISTINCT") => Sql("COUNT(DISTINCT {0})"),
        (PostgreSQL, "APPROX_PERCENTILE") => Sql("PERCENTILE_CONT({1}) WITHIN GROUP (ORDER BY {0})"),
        (PostgreSQL, "DATEADD:YEAR") => Sql("{1} + {0} * INTERVAL '1 YEAR'"),
        (PostgreSQL, "DATEADD:QUARTER") => Sql("{1} + 3 * {0} * INTERVAL '1 MONTH'"),
        (PostgreSQL, "DATEADD:MONTH") => Sql("{1} + {0} * INTERVAL '1 MONTH'"),
        (PostgreSQL, "DATEADD:WEEK") => Sql("{1} + {0} * INTERVAL '7 DAY'"),
        (PostgreSQL, "DATEADD:DAY") => Sql("{1} + {0} * INTERVAL '1 DAY'"),
        (PostgreSQL, "DATEADD:HOUR") => Sql("{1} + {0} * INTERVAL '1 HOUR'"),
        (PostgreSQL, "DATEADD:MINUTE") => Sql("{1} + {0} * INTERVAL '1 MINUTE'"),
        (PostgreSQL, "DATEADD:SECOND") => Sql("{1} + {0} * INTERVAL '1 SECOND'"),
        (PostgreSQL, "DATEDIFF:YEAR") => Sql("CAST(EXTRACT(YEAR FROM {1}) - EXTRACT(YEAR FROM {0}) AS BIGINT)"),
        (PostgreSQL, "DATEDIFF:QUARTER") => Sql("CAST(4 * (EXTRACT(YEAR FROM {1}) - EXTRACT(YEAR FROM {0})) + EXTRACT(QUARTER FROM {1}) - EXTRACT(QUARTER FROM {0}) AS BIGINT)"),
        (PostgreSQL, "DATEDIFF:MONTH") => Sql("CAST(12 * (EXTRACT(YEAR FROM {1}) - EXTRACT(YEAR FROM {0})) + EXTRACT(MONTH FROM {1}) - EXTRACT(MONTH FROM {0}) AS BIGINT)"),
        (PostgreSQL, "DATEDIFF:WEEK") => Sql("(CAST(DATE_TRUNC('WEEK', {1}) AS DATE) - CAST(DATE_TRUNC('WEEK', {0}) AS DATE)) / 7"),
        (PostgreSQL, "DATEDIFF:DAY") => Sql("CAST({1} AS DATE) - CAST({0} AS DATE)"),
        (PostgreSQL, "DATEDIFF:HOUR") => Sql("CAST(EXTRACT(EPOCH FROM DATE_TRUNC('HOUR', {1}) - DATE_TRUNC('HOUR', {0})) / 3600 AS BIGINT)"),
        (PostgreSQL, "DATEDIFF:MINUTE") => Sql("CAST(EXTRACT(EPOCH FROM DATE_TRUNC('MINUTE', {1}) - DATE_TRUNC('MINUTE', {0})) / 60 AS BIGINT)"),
        (PostgreSQL, "DATEDIFF:SECOND") => Sql("CAST(EXTRACT(EPOCH FROM DATE_TRUNC('SECOND', {1}) - DATE_TRUNC('SECOND', {0})) AS BIGINT)"),

        // MySQL
        (MySQL, "LENGTH") => Sql("__ossiec_exact_CHAR_LENGTH({0})"),
        // Grouped by a bare string length, StarRocks returned 1 for a length of 5,
        // apparently its dictionary code. Evidence: `LENGTH(s)`.
        (StarRocks, "LENGTH") => Sql("__ossiec_exact_CHAR_LENGTH({0}) + 0"),
        (MySQL | StarRocks, "STARTSWITH") => Sql("LEFT({0}, __ossiec_exact_CHAR_LENGTH({1})) = {1}"),
        (MySQL | StarRocks, "ENDSWITH") => Sql("RIGHT({0}, __ossiec_exact_CHAR_LENGTH({1})) = {1}"),
        (MySQL, "TRY_CAST") => Refuse,
        (MySQL, "SPLIT_PART") => Sql("CASE WHEN {0} IS NULL THEN NULL WHEN {2} <= (__ossiec_exact_CHAR_LENGTH({0}) - __ossiec_exact_CHAR_LENGTH(REPLACE({0}, {1}, ''))) / __ossiec_exact_CHAR_LENGTH({1}) + 1 THEN SUBSTRING_INDEX(SUBSTRING_INDEX({0}, {1}, {2}), {1}, -1) ELSE '' END"),
        (MySQL, "REGEXP_EXTRACT") => Sql("REGEXP_SUBSTR({0}, {1})"),
        (MySQL, "REGEXP_COUNT") => Refuse,
        (MySQL, "MEDIAN" | "PERCENTILE_CONT" | "PERCENTILE_DISC" | "APPROX_PERCENTILE") => Refuse,
        (MySQL, "APPROX_COUNT_DISTINCT") => Sql("COUNT(DISTINCT {0})"),
        (MySQL | StarRocks, "EXTRACT:WEEK") => Sql("WEEK({0}, 3)"),
        (MySQL | StarRocks, "EXTRACT:DAYOFWEEK") => Sql("DAYOFWEEK({0}) - 1"),
        (MySQL | StarRocks, "EXTRACT:DAYOFYEAR") => Sql("DAYOFYEAR({0})"),
        (MySQL, "EXTRACT:MILLISECOND") => Sql("SECOND({0}) * 1000 + MICROSECOND({0}) / 1000"),
        (MySQL, "DATE_TRUNC:YEAR") => Sql("CAST(DATE_FORMAT({0}, '%Y-01-01') AS DATE)"),
        (MySQL, "DATE_TRUNC:QUARTER") => Sql("CAST(DATE_FORMAT({0}, '%Y-01-01') AS DATE) + INTERVAL 3 * (QUARTER({0}) - 1) MONTH"),
        (MySQL, "DATE_TRUNC:MONTH") => Sql("CAST(DATE_FORMAT({0}, '%Y-%m-01') AS DATE)"),
        (MySQL, "DATE_TRUNC:WEEK") => Sql("CAST({0} AS DATE) - INTERVAL WEEKDAY({0}) DAY"),
        (MySQL, "DATE_TRUNC:DAY") => Sql("CAST({0} AS DATE)"),
        (MySQL, "DATE_TRUNC:HOUR") => Sql("CAST(DATE_FORMAT({0}, '%Y-%m-%d %H:00:00') AS DATETIME)"),
        (MySQL, "DATE_TRUNC:MINUTE") => Sql("CAST(DATE_FORMAT({0}, '%Y-%m-%d %H:%i:00') AS DATETIME)"),
        (MySQL, "DATE_TRUNC:SECOND") => Sql("CAST(DATE_FORMAT({0}, '%Y-%m-%d %H:%i:%s') AS DATETIME)"),
        (MySQL | StarRocks, "DATEADD:YEAR") => Sql("{1} + INTERVAL {0} YEAR"),
        (MySQL | StarRocks, "DATEADD:QUARTER") => Sql("{1} + INTERVAL {0} QUARTER"),
        (MySQL | StarRocks, "DATEADD:MONTH") => Sql("{1} + INTERVAL {0} MONTH"),
        (MySQL | StarRocks, "DATEADD:WEEK") => Sql("{1} + INTERVAL {0} WEEK"),
        (MySQL | StarRocks, "DATEADD:DAY") => Sql("{1} + INTERVAL {0} DAY"),
        (MySQL | StarRocks, "DATEADD:HOUR") => Sql("{1} + INTERVAL {0} HOUR"),
        (MySQL | StarRocks, "DATEADD:MINUTE") => Sql("{1} + INTERVAL {0} MINUTE"),
        (MySQL | StarRocks, "DATEADD:SECOND") => Sql("{1} + INTERVAL {0} SECOND"),
        (MySQL | StarRocks, "DATEDIFF:YEAR") => Sql("CAST(YEAR({1}) AS BIGINT) - CAST(YEAR({0}) AS BIGINT)"),
        (MySQL | StarRocks, "DATEDIFF:QUARTER") => Sql("4 * (CAST(YEAR({1}) AS BIGINT) - CAST(YEAR({0}) AS BIGINT)) + CAST(QUARTER({1}) AS BIGINT) - CAST(QUARTER({0}) AS BIGINT)"),
        (MySQL | StarRocks, "DATEDIFF:MONTH") => Sql("12 * (CAST(YEAR({1}) AS BIGINT) - CAST(YEAR({0}) AS BIGINT)) + CAST(MONTH({1}) AS BIGINT) - CAST(MONTH({0}) AS BIGINT)"),
        (MySQL | StarRocks, "DATEDIFF:WEEK") => Sql("FLOOR((TO_DAYS({1}) - WEEKDAY({1}) - TO_DAYS({0}) + WEEKDAY({0})) / 7)"),
        (MySQL | StarRocks, "DATEDIFF:DAY") => Sql("TO_DAYS({1}) - TO_DAYS({0})"),
        (MySQL | StarRocks, "DATEDIFF:HOUR") => Sql("TIMESTAMPDIFF(HOUR, DATE_FORMAT({0}, '%Y-%m-%d %H:00:00'), DATE_FORMAT({1}, '%Y-%m-%d %H:00:00'))"),
        (MySQL | StarRocks, "DATEDIFF:MINUTE") => Sql("TIMESTAMPDIFF(MINUTE, DATE_FORMAT({0}, '%Y-%m-%d %H:%i:00'), DATE_FORMAT({1}, '%Y-%m-%d %H:%i:00'))"),
        (MySQL | StarRocks, "DATEDIFF:SECOND") => Sql("TIMESTAMPDIFF(SECOND, DATE_FORMAT({0}, '%Y-%m-%d %H:%i:%s'), DATE_FORMAT({1}, '%Y-%m-%d %H:%i:%s'))"),
        (MySQL | StarRocks, "TO_CHAR:2") => Sql("DATE_FORMAT({0}, {1})"),
        (MySQL | StarRocks, "TO_DATE:2") => Sql("CAST(STR_TO_DATE({0}, {1}) AS DATE)"),
        (MySQL | StarRocks, "TO_TIMESTAMP:2") => Sql("CAST(STR_TO_DATE({0}, {1}) AS DATETIME)"),

        // StarRocks. `||` is logical OR, as on MySQL by default.
        (StarRocks, "SPLIT_PART") => Sql("CASE WHEN {0} IS NULL THEN NULL ELSE __ossiec_exact_SPLIT_PART({0}, {1}, {2}) END"),
        (StarRocks, "CONCAT_OP") => Sql("CONCAT({0}, {1})"),
        (StarRocks, "TRUNC") => Sql("__ossiec_exact_TRUNCATE({0}, {1})"),
        (StarRocks, "REGEXP_EXTRACT") => Sql("REGEXP_EXTRACT({0}, {1}, 0)"),
        (StarRocks, "EXTRACT:MILLISECOND") => Sql("SECOND({0}) * 1000 + CAST(DATE_FORMAT({0}, '%f') AS BIGINT) / 1000"),
        (StarRocks, "TIME_LITERAL") => Sql("CAST({0} AS TIME)"),
        (StarRocks, "NULL_SAFE_EQ") => Sql("CASE WHEN {0} IS NULL AND {1} IS NULL THEN TRUE WHEN {0} IS NULL OR {1} IS NULL THEN FALSE ELSE {0} = {1} END"),
        (StarRocks, "NULL_SAFE_NEQ") => Sql("CASE WHEN {0} IS NULL AND {1} IS NULL THEN FALSE WHEN {0} IS NULL OR {1} IS NULL THEN TRUE ELSE {0} <> {1} END"),
        (StarRocks, "MEDIAN") => Sql("__ossiec_exact_PERCENTILE_CONT({0}, 0.5)"),
        (StarRocks, "PERCENTILE_CONT") => Sql("__ossiec_exact_PERCENTILE_CONT({1}, {0})"),
        // Its own `PERCENTILE_DISC` differs from the standard for many sizes and
        // percentiles. Checked against Postgres. NULLs sort first, so they're skipped.
        (StarRocks, "PERCENTILE_DISC") => Sql("ELEMENT_AT(ARRAY_SORT(ARRAY_AGG({1})), CAST(COUNT(*) - COUNT({1}) + GREATEST(CEIL({0} * COUNT({1})), 1) AS INT))"),
        (StarRocks, "APPROX_PERCENTILE") => Sql("PERCENTILE_APPROX({0}, {1})"),

        // Trino has only an approximate percentile, which polyglot would use for the
        // exact ones. Its `date_diff` counts whole units; truncating both ends counts
        // the boundaries crossed. Its `FLOAT` is `REAL`, and the language's is 64-bit.
        (Trino, "MEDIAN" | "PERCENTILE_CONT" | "PERCENTILE_DISC") => Refuse,
        (Trino, "APPROX_PERCENTILE") => Sql("__ossiec_exact_APPROX_PERCENTILE({0}, {1})"),
        // Sunday is 7 on Trino, and 0 on every other engine here.
        (Trino, "EXTRACT:DAYOFWEEK") => Sql("EXTRACT(DOW FROM {0}) % 7"),
        (Trino, "EXTRACT:MILLISECOND") => Sql("SECOND({0}) * 1000 + MILLISECOND({0})"),
        (Trino, "REGEXP_EXTRACT") => Sql("__ossiec_exact_REGEXP_EXTRACT({0}, {1})"),
        (Trino, "TRUNC") => Sql("TRUNCATE(CAST({0} AS DECIMAL(38, 10)), {1})"),
        // Past the last part it returns NULL, and the other engines here an empty string.
        (Trino, "SPLIT_PART") => Sql("CASE WHEN {0} IS NULL THEN NULL ELSE COALESCE(__ossiec_exact_SPLIT_PART({0}, {1}, {2}), '') END"),
        (Trino, "CAST_FLOAT") => Sql("CAST({0} AS DOUBLE)"),
        (Trino, "DATEDIFF:YEAR") => Sql("__ossiec_exact_DATE_DIFF('YEAR', DATE_TRUNC('YEAR', {0}), DATE_TRUNC('YEAR', {1}))"),
        (Trino, "DATEDIFF:QUARTER") => Sql("__ossiec_exact_DATE_DIFF('QUARTER', DATE_TRUNC('QUARTER', {0}), DATE_TRUNC('QUARTER', {1}))"),
        (Trino, "DATEDIFF:MONTH") => Sql("__ossiec_exact_DATE_DIFF('MONTH', DATE_TRUNC('MONTH', {0}), DATE_TRUNC('MONTH', {1}))"),
        (Trino, "DATEDIFF:WEEK") => Sql("__ossiec_exact_DATE_DIFF('WEEK', DATE_TRUNC('WEEK', {0}), DATE_TRUNC('WEEK', {1}))"),
        (Trino, "DATEDIFF:DAY") => Sql("__ossiec_exact_DATE_DIFF('DAY', DATE_TRUNC('DAY', {0}), DATE_TRUNC('DAY', {1}))"),
        (Trino, "DATEDIFF:HOUR") => Sql("__ossiec_exact_DATE_DIFF('HOUR', DATE_TRUNC('HOUR', {0}), DATE_TRUNC('HOUR', {1}))"),
        (Trino, "DATEDIFF:MINUTE") => Sql("__ossiec_exact_DATE_DIFF('MINUTE', DATE_TRUNC('MINUTE', {0}), DATE_TRUNC('MINUTE', {1}))"),
        (Trino, "DATEDIFF:SECOND") => Sql("__ossiec_exact_DATE_DIFF('SECOND', DATE_TRUNC('SECOND', {0}), DATE_TRUNC('SECOND', {1}))"),
        (Trino, "TO_CHAR:2") => Sql("__ossiec_exact_DATE_FORMAT(CAST({0} AS TIMESTAMP), {1})"),
        (Trino, "TO_DATE:2") => Sql("CAST(__ossiec_exact_DATE_PARSE({0}, {1}) AS DATE)"),
        (Trino, "TO_TIMESTAMP:2") => Sql("__ossiec_exact_DATE_PARSE({0}, {1})"),

        // ClickHouse: function names are case-sensitive, and the plain string functions
        // count and change bytes, so non-ASCII text needs the UTF-8 ones.
        (ClickHouse, "UPPER") => Sql("upperUTF8({0})"),
        (ClickHouse, "LOWER") => Sql("lowerUTF8({0})"),
        (ClickHouse, "LEFT") => Sql("leftUTF8({0}, {1})"),
        (ClickHouse, "RIGHT") => Sql("rightUTF8({0}, {1})"),
        (ClickHouse, "SUBSTRING") => Sql("substringUTF8({0}, {1}, {2})"),
        (ClickHouse, "POSITION") => Sql("positionUTF8({1}, {0})"),
        (ClickHouse, "LENGTH") => Sql("lengthUTF8({0})"),
        (ClickHouse, "STARTSWITH") => Sql("startsWith({0}, {1})"),
        (ClickHouse, "ENDSWITH") => Sql("endsWith({0}, {1})"),
        (ClickHouse, "CHARINDEX") => Sql("positionUTF8({1}, {0})"),
        (ClickHouse, "CONTAINS") => Sql("positionUTF8({0}, {1}) > 0"),
        (ClickHouse, "SPLIT_PART") => Sql("CASE WHEN {0} IS NULL THEN NULL ELSE arrayElement(splitByString({1}, {0}), {2}) END"),
        (ClickHouse, "REGEXP_COUNT") => Sql("countMatches({0}, {1})"),
        (ClickHouse, "CURRENT_TIME") => Sql("toTime(now())"),
        (ClickHouse, "TIME_LITERAL") => Sql("CAST({0} AS TIME)"),
        (ClickHouse, "EXTRACT:MILLISECOND") => Sql("toSecond({0}) * 1000 + toMillisecond({0})"),
        (ClickHouse, "EXTRACT:DAYOFWEEK") => Sql("toDayOfWeek({0}, 3) - 1"),
        (ClickHouse, "EXTRACT:DAYOFYEAR") => Sql("toDayOfYear({0})"),
        (ClickHouse, "APPROX_COUNT_DISTINCT") => Sql("uniq({0})"),
        // polyglot can't read ClickHouse's `f(p)(x)`, which `arrayReduce` also spells.
        (ClickHouse, "MEDIAN") => Sql("medianExactInclusive_array"),
        (ClickHouse, "PERCENTILE_CONT") => Sql("quantileExactInclusive_array"),
        // Its `quantileExactLow` and `quantileExactHigh` each differ from the standard
        // for some sizes and percentiles. This is the standard's definition: the first
        // sorted value at or past the percentile. Checked against Postgres.
        (ClickHouse, "PERCENTILE_DISC") => Sql("CASE WHEN COUNT({1}) = 0 THEN NULL ELSE arrayElement(arraySort(groupArray({1})), toUInt64(GREATEST(CEIL(toDecimal64({0}, 6) * toDecimal64(COUNT({1}), 0)), 1))) END"),
        (ClickHouse, "APPROX_PERCENTILE") => Sql("quantile_array"),
        (ClickHouse, "CUME_DIST") => Sql("__ossiec_exact_cume_dist()"),
        (ClickHouse, "TO_CHAR:2") => Sql("formatDateTime({0}, {1})"),
        (ClickHouse, "TO_DATE:2") => Sql("toDate(parseDateTime({0}, {1}))"),
        (ClickHouse, "TO_TIMESTAMP:2") => Sql("parseDateTime({0}, {1})"),
        (ClickHouse, "LOG") => Sql("ln({1}) / ln({0})"),
        _ => return None,
    })
}

/// Format tokens (expression language, "Date Formatting"), longest first so that
/// `MONTH` isn't read as `MON`, with what each engine calls them.
const FORMAT_TOKENS: &[(&str, &str, &str)] = &[
    // token, strftime (DuckDB), MySQL and ClickHouse
    ("YYYY", "%Y", "%Y"),
    ("MONTH", "%B", "%M"),
    ("HH24", "%H", "%H"),
    ("HH12", "%I", "%I"),
    ("MON", "%b", "%b"),
    ("DAY", "%A", "%W"),
    ("YY", "%y", "%y"),
    ("MM", "%m", "%m"),
    ("DD", "%d", "%d"),
    ("DY", "%a", "%a"),
    ("HH", "%I", "%I"),
    ("MI", "%M", "%i"),
    ("SS", "%S", "%S"),
    ("AM", "%p", "%p"),
    ("PM", "%p", "%p"),
];

fn translate_format(format: &str, strftime: bool) -> String {
    let mut out = String::with_capacity(format.len());
    let mut rest = format;
    'scan: while let Some(c) = rest.chars().next() {
        for (token, s, m) in FORMAT_TOKENS {
            if let Some(after) = rest.strip_prefix(token) {
                out.push_str(if strftime { s } else { m });
                rest = after;
                continue 'scan;
            }
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn name_of(key: &str) -> &str {
    key.split(':').next().unwrap_or(key)
}

fn engine_name(target: DialectType) -> String {
    format!("{target:?}")
}

/// Give every Ossie function its implementation for `target`.
pub(crate) fn lower(ast: Expression, target: DialectType) -> Result<Expression> {
    use DialectType::{ClickHouse, DuckDB, MySQL, StarRocks, Trino};
    let failure = RefCell::new(None);
    let out = transform(ast, &|e| {
        let Some(mut c) = ossie_call(&e) else {
            return Ok(Some(e));
        };
        let rule = on_engine(target, &c.key).or_else(|| standard(&c.key).map(Sql));
        let sql = match rule {
            Some(Sql(s)) => s,
            Some(Refuse) => {
                failure
                    .borrow_mut()
                    .get_or_insert(Error::Unsupported(format!(
                        "{} has no equivalent on {} that gives the same answer",
                        name_of(&c.key),
                        engine_name(target)
                    )));
                return Ok(Some(e));
            }
            None => return Ok(Some(e)),
        };
        // A format string is Ossie's, and only a constant one can be translated.
        if matches!(target, DuckDB | MySQL | ClickHouse | StarRocks | Trino)
            && matches!(c.key.as_str(), "TO_CHAR:2" | "TO_DATE:2" | "TO_TIMESTAMP:2")
        {
            let Some(format) = text(&c.args[1]).map(str::to_string) else {
                failure
                    .borrow_mut()
                    .get_or_insert(Error::Unsupported(format!(
                        "{} on {} needs a constant format string",
                        name_of(&c.key),
                        engine_name(target)
                    )));
                return Ok(Some(e));
            };
            let format = translate_format(&format, target == DuckDB);
            let format = if matches!(target, MySQL | StarRocks | Trino) {
                format.replace("%S", "%s")
            } else {
                format
            };
            c.args[1] = Expression::Literal(Box::new(Literal::String(format)));
        }
        let sql = match quantile_on_clickhouse(sql, &c.args) {
            Some(s) => s,
            None if sql.ends_with("_array") => {
                failure
                    .borrow_mut()
                    .get_or_insert(Error::Unsupported(format!(
                        "{} on {} needs a constant percentile",
                        name_of(&c.key),
                        engine_name(target)
                    )));
                return Ok(Some(e));
            }
            None => sql.to_string(),
        };
        let sql = if sql == CONCAT_BY_OPERATOR {
            (0..c.args.len())
                .map(|i| format!("{{{i}}}"))
                .collect::<Vec<_>>()
                .join(" || ")
        } else {
            sql
        };
        let mut t = Tpl::new();
        let mut text = sql;
        for (i, arg) in c.args.iter().enumerate() {
            let slot = format!("{{{i}}}");
            while text.contains(&slot) {
                let ph = t.e(arg.clone());
                text = text.replacen(&slot, &ph, 1);
            }
        }
        // In parentheses, so it stays one operand wherever the call sat:
        // `2 * EXTRACT(DOW ...)` must not become `2 * DAYOFWEEK(d) - 1`.
        t.push(&format!("({text})"));
        let built = without_needless_parens(
            t.build()
                .map_err(|err| polyglot_sql::Error::internal(err.to_string()))?,
        );
        Ok(Some(match c.filter {
            None => built,
            Some(p) => with_filter(built, p),
        }))
    })
    .map_err(|e| Error::Internal(e.to_string()))?;
    failure.into_inner().map_or(Ok(out), Err)
}

/// `arrayReduce('quantile(p)', groupArray(x))`. The function and its constant
/// percentile are one string, so `sql` names the function and this writes the call.
/// `groupArray` skips NULL, as a percentile must, and with no rows the answer is NULL
/// rather than the `nan` `arrayReduce` gives.
fn quantile_on_clickhouse(sql: &str, args: &[Expression]) -> Option<String> {
    let function = sql.strip_suffix("_array")?;
    let number = |e: &Expression| match e {
        Expression::Literal(l) => match l.as_ref() {
            Literal::Number(n) => Some(n.clone()),
            _ => None,
        },
        _ => None,
    };
    let (function, p, x) = match function {
        "medianExactInclusive" => ("quantileExactInclusive", "0.5".to_string(), "{0}"),
        "quantile" => ("quantile", number(&args[1])?, "{0}"),
        f => (f, number(&args[0])?, "{1}"),
    };
    Some(format!(
        "CASE WHEN COUNT({x}) = 0 THEN NULL ELSE arrayReduce('{function}({p})', groupArray({x})) END"
    ))
}

/// A call or a `CASE` is already one operand.
fn without_needless_parens(e: Expression) -> Expression {
    match e {
        Expression::Paren(p)
            if !matches!(
                p.this,
                Expression::Add(_)
                    | Expression::Sub(_)
                    | Expression::Mul(_)
                    | Expression::Div(_)
                    | Expression::Mod(_)
                    | Expression::Eq(_)
                    | Expression::Neq(_)
                    | Expression::Gt(_)
                    | Expression::Gte(_)
                    | Expression::Lt(_)
                    | Expression::Lte(_)
                    | Expression::Concat(_)
                    | Expression::IntDiv(_)
            ) =>
        {
            p.this
        }
        other => other,
    }
}

/// `FILTER` on a rewritten aggregate: on the node where it can be read back.
fn with_filter(e: Expression, p: Expression) -> Expression {
    match e {
        Expression::Count(mut c) => {
            c.filter = Some(p);
            Expression::Count(c)
        }
        Expression::AggregateFunction(mut a) => {
            a.filter = Some(p);
            Expression::AggregateFunction(a)
        }
        other => Expression::Filter(Box::new(polyglot_sql::expressions::Filter {
            this: Box::new(other),
            expression: Box::new(p),
        })),
    }
}
