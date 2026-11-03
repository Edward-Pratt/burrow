//! What a database session answers: query results and errors, the
//! schema, and the node's query history.

use serde::{Deserialize, Serialize};

/// What a query sent with [`Command::RunQuery`](crate::api::Command::RunQuery) returned. A query can hold
/// several statements; each one that finished has an entry in
/// `statements`, in order. If one failed, `error` says why and the
/// statements after it did not run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryResult {
    pub statements: Vec<StatementResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<QueryError>,
    /// Set when Burrow stopped the query because it returned more rows
    /// than the limit, or more than 32 MiB of them as JSON. The statement
    /// that hit it is `truncated`, and anything after it in the same query
    /// did not run.
    #[serde(default)]
    pub stopped: bool,
    /// How long the query took, in milliseconds.
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatementResult {
    /// Empty for a statement that returns no rows, such as an `UPDATE`.
    pub columns: Vec<String>,
    /// Every value as the server prints it, `None` for NULL. Values can
    /// hold any character, so clients must clean them before showing them.
    pub rows: Vec<Vec<Option<String>>>,
    /// Rows the statement changed or returned, as the server reported it.
    /// Zero when the statement was stopped.
    pub count: u64,
    /// More rows were left out than `rows` holds.
    #[serde(default)]
    pub truncated: bool,
}

/// An error the database server reported.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Where in the query the error is, counted in characters from 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
    /// The SQLSTATE code, such as `42P01`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// A schema and the tables, views and other relations in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbSchema {
    pub name: String,
    pub tables: Vec<DbTable>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbTable {
    pub name: String,
    pub kind: DbTableKind,
    pub columns: Vec<DbColumn>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbTableKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
    PartitionedTable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbColumn {
    pub name: String,
    /// As the server spells it, such as `character varying(40)`.
    pub data_type: String,
    pub nullable: bool,
}

/// A query run on a database node, kept on this computer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub sql: String,
    /// When it last ran, in seconds since the Unix epoch.
    pub at: u64,
    /// Whether it ran without an error.
    pub ok: bool,
}
