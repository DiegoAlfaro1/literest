//! Table introspection and the Postgres <-> SQLite value mapping.
//!
//! SQLite has five storage classes and no real column types, so the declared
//! type in `CREATE TABLE` is the only place the Postgres-side intent survives
//! (`BOOLEAN`, `TIMESTAMPTZ`, `JSONB`, ...). Everything here keys off that.

use rusqlite::Connection;
use rusqlite::types::{Value, ValueRef};
use serde_json::Value as Json;

use crate::ApiError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bool,
    Int,
    Real,
    Text,
    Json,
    Timestamptz,
    Timestamp,
    Date,
}

impl Kind {
    pub fn from_decl(decl: &str) -> Kind {
        let t = decl.to_ascii_uppercase();
        if t.contains("BOOL") {
            Kind::Bool
        } else if t.contains("TIMESTAMPTZ") || t.contains("WITH TIME ZONE") {
            Kind::Timestamptz
        } else if t.contains("TIMESTAMP") || t.contains("DATETIME") {
            Kind::Timestamp
        } else if t == "DATE" {
            Kind::Date
        } else if t.contains("JSON") {
            Kind::Json
        } else if t.contains("INT") {
            Kind::Int
        } else if ["REAL", "FLOA", "DOUB", "NUMERIC", "DECIMAL"].iter().any(|k| t.contains(k)) {
            Kind::Real
        } else {
            Kind::Text
        }
    }

    fn pg_name(self) -> &'static str {
        match self {
            Kind::Bool => "boolean",
            Kind::Int => "bigint",
            Kind::Real => "double precision",
            Kind::Text => "text",
            Kind::Json => "jsonb",
            Kind::Timestamptz => "timestamp with time zone",
            Kind::Timestamp => "timestamp without time zone",
            Kind::Date => "date",
        }
    }
}

pub struct Column {
    pub name: String,
    pub kind: Kind,
}

/// The table's columns in declaration order. Doubles as the table-name
/// allowlist: a table only exists for us if `pragma_table_info` knows it.
pub fn columns(conn: &Connection, table: &str) -> Result<Vec<Column>, ApiError> {
    let missing = || {
        ApiError::new(
            404,
            "PGRST205",
            format!("Could not find the table 'public.{table}' in the schema cache"),
        )
    };
    if table.starts_with("sqlite_") {
        return Err(missing());
    }
    let mut stmt = conn.prepare_cached("SELECT name, type FROM pragma_table_info(?1)")?;
    let cols = stmt
        .query_map([table], |r| {
            let decl: Option<String> = r.get(1)?;
            Ok(Column { name: r.get(0)?, kind: Kind::from_decl(&decl.unwrap_or_default()) })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if cols.is_empty() { Err(missing()) } else { Ok(cols) }
}

/// Only ever called on names that matched a real table or column.
pub fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// A filter value from the URL (always text) -> the SQL value to bind.
pub fn from_text(conn: &Connection, kind: Kind, s: &str) -> Result<Value, ApiError> {
    Ok(match kind {
        Kind::Bool => match s.to_ascii_lowercase().as_str() {
            "t" | "true" | "y" | "yes" | "on" | "1" => Value::Integer(1),
            "f" | "false" | "n" | "no" | "off" | "0" => Value::Integer(0),
            _ => return Err(invalid(kind, s)),
        },
        Kind::Int => Value::Integer(s.trim().parse().map_err(|_| invalid(kind, s))?),
        Kind::Real => Value::Real(s.trim().parse().map_err(|_| invalid(kind, s))?),
        Kind::Timestamptz | Kind::Timestamp | Kind::Date => Value::Text(normalize_time(conn, kind, s)?),
        Kind::Text | Kind::Json => Value::Text(s.to_string()),
    })
}

/// A value from a JSON request body -> the SQL value to bind.
pub fn from_json(conn: &Connection, kind: Kind, v: &Json) -> Result<Value, ApiError> {
    Ok(match (kind, v) {
        (_, Json::Null) => Value::Null,
        (Kind::Json, v) => Value::Text(v.to_string()),
        (_, Json::String(s)) => return from_text(conn, kind, s),
        (Kind::Bool, Json::Bool(b)) => Value::Integer(*b as i64),
        (Kind::Int, Json::Number(n)) => Value::Integer(n.as_i64().ok_or_else(|| invalid(kind, &n.to_string()))?),
        (Kind::Real, Json::Number(n)) => Value::Real(n.as_f64().unwrap_or_default()),
        (Kind::Text, Json::Number(n)) => Value::Text(n.to_string()),
        (Kind::Text, Json::Bool(b)) => Value::Text(b.to_string()),
        _ => return Err(invalid(kind, &v.to_string())),
    })
}

/// A stored SQLite value -> what PostgREST would have put in the JSON.
pub fn to_json(kind: Kind, v: ValueRef) -> Json {
    match (kind, v) {
        (_, ValueRef::Null) => Json::Null,
        (Kind::Bool, ValueRef::Integer(i)) => Json::Bool(i != 0),
        (Kind::Json, ValueRef::Text(t)) => serde_json::from_slice(t).unwrap_or_else(|_| text(t)),
        (_, ValueRef::Integer(i)) => i.into(),
        (_, ValueRef::Real(f)) => serde_json::Number::from_f64(f).map_or(Json::Null, Json::Number),
        (_, ValueRef::Text(t)) => text(t),
        // Same "\x..." hex form Postgres uses for bytea.
        (_, ValueRef::Blob(b)) => {
            Json::String(format!("\\x{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()))
        }
    }
}

fn text(t: &[u8]) -> Json {
    Json::String(String::from_utf8_lossy(t).into_owned())
}

/// Timestamps are stored as text in exactly the shape PostgREST returns them
/// (`2024-01-15T10:30:00.5+00:00` for timestamptz, always UTC), so output needs
/// no conversion and text comparison gives chronological order.
fn normalize_time(conn: &Connection, kind: Kind, s: &str) -> Result<String, ApiError> {
    // ponytail: SQLite's own date functions do the parsing (ISO-8601, `Z` and
    // ±HH:MM offsets, converted to UTC). They keep milliseconds only; swap in
    // chrono if microsecond precision or more input formats matter.
    let sql = if kind == Kind::Date { "SELECT date(?1)" } else { "SELECT strftime('%Y-%m-%dT%H:%M:%f', ?1)" };
    let parsed: Option<String> = conn.prepare_cached(sql)?.query_row([s], |r| r.get(0))?;
    let mut out = parsed.ok_or_else(|| invalid(kind, s))?;
    if kind != Kind::Date {
        // `%f` is always "SS.SSS"; Postgres drops trailing fractional zeros.
        out = out.trim_end_matches('0').trim_end_matches('.').to_string();
        if kind == Kind::Timestamptz {
            out.push_str("+00:00");
        }
    }
    Ok(out)
}

fn invalid(kind: Kind, s: &str) -> ApiError {
    let code = if matches!(kind, Kind::Timestamptz | Kind::Timestamp | Kind::Date) { "22007" } else { "22P02" };
    ApiError::bad(code, format!("invalid input syntax for type {}: \"{s}\"", kind.pg_name()))
}
