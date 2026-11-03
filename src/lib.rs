//! # ossiec
//!
//! A compiler from [Apache Ossie](https://ossie.apache.org/) semantic models to
//! correct, engine-native SQL.
//!
//! ```
//! let graph = ossiec::from_yaml(r#"
//! version: "0.2.0.dev0"
//! name: shop
//! datasets:
//!   - name: orders
//!     source: sales.orders
//!     primary_key: [id]
//!     fields:
//!       - { name: id, expression: { sql: order_id } }
//!       - { name: region, expression: { sql: region } }
//!       - { name: amount, expression: { sql: amount_cents / 100.0 } }
//! metrics:
//!   - { name: revenue, expression: { sql: SUM(orders.amount) } }
//! "#).unwrap();
//!
//! let request = serde_json::from_str(r#"{"metrics": ["revenue"], "dimensions": ["orders.region"]}"#).unwrap();
//! let compiled = ossiec::compile(&graph, &request, &ossiec::Options::default()).unwrap();
//! assert!(compiled.sql.contains("SUM"));
//! ```
//!
//! The pipeline is `load` → `bind` → `compile` (plan, then lower). See the
//! README for the planning rules that keep metrics free of fan-out and chasm traps.

mod dialect;
mod graph;
mod layer2;
mod lower;
pub mod model;
mod plan;
mod sql;

pub use graph::{bind, Catalog, DimensionDoc, MetricDoc, SemanticGraph};
pub use layer2::{compile_layer2, to_request as layer2_to_request};
pub use model::Model;
pub use plan::{Direction, Filter, MetricFilter, Nulls, Op, OrderBy, Request, TimeGrain};
pub use polyglot_sql::DialectType;

use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The document isn't an Ossie semantic model ossiec can read.
    #[error("cannot load model: {0}")]
    Load(String),
    /// The model breaks Ossie rules. Every problem is listed.
    #[error("invalid model:\n  - {}", .0.join("\n  - "))]
    InvalidModel(Vec<String>),
    /// The request doesn't fit the model (unknown name, bad filter, ...).
    #[error("invalid request: {0}")]
    Request(String),
    /// Valid Ossie that this version can't compile yet.
    #[error("not supported yet: {0}")]
    Unsupported(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Parse an Ossie YAML (or JSON) document.
pub fn load(yaml: &str) -> Result<Model> {
    let v: serde_yaml::Value =
        serde_yaml::from_str(yaml).map_err(|e| Error::Load(e.to_string()))?;
    let m = v
        .as_mapping()
        .ok_or_else(|| Error::Load("expected a mapping at the document root".into()))?;
    let has = |k: &str| m.contains_key(serde_yaml::Value::from(k));
    if has("semantic_model") {
        return Err(Error::Load(
            "this is a pre-0.2 Ossie document (a `semantic_model:` array); ossiec reads the current spec, where the model sits at the document root".into(),
        ));
    }
    if !has("datasets") {
        let what = if has("ontology") {
            "an Ossie ontology document"
        } else {
            "not an Ossie semantic model"
        };
        return Err(Error::Load(format!("no `datasets` key: this is {what}")));
    }
    serde_yaml::from_value(v).map_err(|e| Error::Load(e.to_string()))
}

/// [`load`] then [`bind`].
pub fn from_yaml(yaml: &str) -> Result<SemanticGraph> {
    bind(load(yaml)?)
}

/// How to render the compiled SQL.
#[derive(Debug, Clone)]
pub struct Options {
    /// Dialect to emit. `None` emits the statement's own source dialect
    /// (`generic`, or the one vendor dialect its expressions are written in),
    /// for callers that translate downstream.
    pub target: Option<DialectType>,
    /// Prefer this Ossie dialect's expressions (`TRINO`, `SNOWFLAKE`, ...) when a field or metric declares one.
    pub prefer_dialect: Option<String>,
    pub pretty: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            target: None,
            prefer_dialect: None,
            pretty: true,
        }
    }
}

/// The result of compiling one request.
#[derive(Debug, Serialize)]
pub struct Compiled {
    pub sql: String,
    /// The dialect `sql` is written in.
    pub dialect: String,
    /// The dialect the model's expressions were read in (`generic` unless a vendor-only expression was used).
    pub source_dialect: String,
    pub model: String,
    pub model_version: Option<String>,
    /// Stable hash of model, version, request and options: same inputs, same SQL.
    pub request_hash: String,
    pub warnings: Vec<String>,
    #[serde(skip)]
    pub ast: polyglot_sql::Expression,
}

/// Compile `request` against `graph` into a single SQL statement.
pub fn compile(graph: &SemanticGraph, request: &Request, opts: &Options) -> Result<Compiled> {
    let mut ctx = graph::Ctx {
        prefer: opts.prefer_dialect.clone(),
        ..Default::default()
    };
    let plan = plan::plan(graph, request, &mut ctx)?;
    let ast = lower::lower(graph, &plan, &mut ctx)?;
    let ast = match opts.target {
        Some(target) => dialect::adapt(ast, target, &plan)?,
        None => ast,
    };
    let (source_name, source) = ctx.source_dialect()?;
    let target = opts.target.unwrap_or(source);
    let mut sql = sql::render(&ast, source, target, opts.pretty)?;
    if let Some(target) = opts.target {
        sql = dialect::finish(sql, target, opts.pretty)?;
    }
    let dialect = if opts.target.is_some() {
        format!("{target:?}").to_ascii_lowercase()
    } else {
        source_name.clone()
    };
    let identity = serde_json::json!([
        graph.model.name,
        graph.model.version,
        request,
        opts.prefer_dialect,
        dialect,
        opts.pretty
    ]);
    Ok(Compiled {
        sql,
        dialect,
        source_dialect: source_name,
        model: graph.model.name.clone(),
        model_version: graph.model.version.clone(),
        request_hash: format!("{:016x}", fnv1a(identity.to_string().as_bytes())),
        warnings: ctx.warnings,
        ast,
    })
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}
