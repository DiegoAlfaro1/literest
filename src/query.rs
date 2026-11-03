//! PostgREST query parameters -> SQL fragments plus bound parameters.
//!
//! Identifiers are only spliced into SQL after matching one of the table's
//! real columns; every value goes through a `?` placeholder.

use rusqlite::Connection;
use rusqlite::types::Value;

use crate::ApiError;
use crate::schema::{self, Column, quote};

pub struct Query {
    /// (key in the JSON response, index into the table's columns)
    pub select: Vec<(String, usize)>,
    pub where_sql: String,
    pub where_params: Vec<Value>,
    pub order_sql: String,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// `columns=` sent by postgrest-js on bulk inserts.
    pub columns: Option<Vec<String>>,
}

impl Query {
    pub fn select_sql(&self, cols: &[Column]) -> String {
        let names: Vec<String> = self.select.iter().map(|(_, i)| quote(&cols[*i].name)).collect();
        names.join(", ")
    }
}

pub fn find<'a>(cols: &'a [Column], name: &str) -> Result<(usize, &'a Column), ApiError> {
    cols.iter()
        .enumerate()
        .find(|(_, c)| c.name == name)
        .ok_or_else(|| ApiError::bad("42703", format!("column \"{name}\" does not exist")))
}

pub fn parse(conn: &Connection, cols: &[Column], params: &[(String, String)]) -> Result<Query, ApiError> {
    let mut q = Query {
        select: all(cols),
        where_sql: String::new(),
        where_params: vec![],
        order_sql: String::new(),
        limit: None,
        offset: None,
        columns: None,
    };
    let mut conds = vec![];
    for (key, val) in params {
        match key.as_str() {
            "select" => q.select = parse_select(cols, val)?,
            "order" => q.order_sql = parse_order(cols, val)?,
            "limit" => q.limit = Some(parse_int(key, val)?),
            "offset" => q.offset = Some(parse_int(key, val)?),
            "columns" => q.columns = Some(val.split(',').map(|c| c.trim().trim_matches('"').to_string()).collect()),
            "or" | "and" => return Err(ApiError::bad("PGRST100", "or/and filters are not supported by literest")),
            _ => conds.push(parse_filter(conn, cols, key, val, &mut q.where_params)?),
        }
    }
    if !conds.is_empty() {
        q.where_sql = format!(" WHERE {}", conds.join(" AND "));
    }
    Ok(q)
}

fn all(cols: &[Column]) -> Vec<(String, usize)> {
    cols.iter().enumerate().map(|(i, c)| (c.name.clone(), i)).collect()
}

fn parse_int(key: &str, val: &str) -> Result<i64, ApiError> {
    val.parse().map_err(|_| ApiError::bad("PGRST100", format!("\"{val}\" is not a valid {key}")))
}

/// `*`, `col`, `alias:col`, comma-separated.
fn parse_select(cols: &[Column], raw: &str) -> Result<Vec<(String, usize)>, ApiError> {
    if raw.contains('(') || raw.contains("->") || raw.contains("::") {
        return Err(ApiError::bad(
            "PGRST100",
            format!("select \"{raw}\": resource embedding, JSON paths and casts are not supported by literest"),
        ));
    }
    let mut out = vec![];
    for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if item == "*" {
            out.extend(all(cols));
            continue;
        }
        let (alias, name) = item.split_once(':').unwrap_or((item, item));
        let (i, _) = find(cols, name.trim_matches('"'))?;
        out.push((alias.trim_matches('"').to_string(), i));
    }
    Ok(if out.is_empty() { all(cols) } else { out })
}

/// `col.asc`, `col.desc.nullsfirst`, ... comma-separated.
fn parse_order(cols: &[Column], raw: &str) -> Result<String, ApiError> {
    let mut terms = vec![];
    for term in raw.split(',') {
        let mut parts = term.trim().split('.');
        let (_, col) = find(cols, parts.next().unwrap_or_default())?;
        let (mut desc, mut nulls_first) = (false, None);
        for modifier in parts {
            match modifier {
                "asc" => desc = false,
                "desc" => desc = true,
                "nullsfirst" => nulls_first = Some(true),
                "nullslast" => nulls_first = Some(false),
                _ => return Err(ApiError::bad("PGRST100", format!("failed to parse order ({raw})"))),
            }
        }
        // Postgres sorts NULL as larger than every value (last ascending, first
        // descending); SQLite does the opposite, so always spell it out.
        let nulls = if nulls_first.unwrap_or(desc) { "FIRST" } else { "LAST" };
        terms.push(format!("{} {} NULLS {nulls}", quote(&col.name), if desc { "DESC" } else { "ASC" }));
    }
    Ok(format!(" ORDER BY {}", terms.join(", ")))
}

/// `col=op.value` or `col=not.op.value` -> one WHERE condition.
fn parse_filter(
    conn: &Connection,
    cols: &[Column],
    key: &str,
    raw: &str,
    params: &mut Vec<Value>,
) -> Result<String, ApiError> {
    let (_, col) = find(cols, key)?;
    let bad = || ApiError::bad("PGRST100", format!("failed to parse filter ({key}={raw})"));
    let (negate, expr) = match raw.strip_prefix("not.") {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let (op, val) = expr.split_once('.').ok_or_else(bad)?;
    let c = quote(&col.name);
    let sql = match op {
        "eq" | "neq" | "gt" | "gte" | "lt" | "lte" => {
            let sym = match op {
                "eq" => "=",
                "neq" => "<>",
                "gt" => ">",
                "gte" => ">=",
                "lt" => "<",
                _ => "<=",
            };
            params.push(schema::from_text(conn, col.kind, val)?);
            format!("{c} {sym} ?")
        }
        // SQLite's LIKE ignores ASCII case (so it is our ilike); GLOB is the case-sensitive one.
        "like" => {
            params.push(Value::Text(like_to_glob(val)));
            format!("{c} GLOB ?")
        }
        "ilike" => {
            params.push(Value::Text(val.replace('*', "%")));
            format!("{c} LIKE ?")
        }
        "in" => {
            let items = parse_list(val).ok_or_else(bad)?;
            for item in &items {
                params.push(schema::from_text(conn, col.kind, item)?);
            }
            format!("{c} IN ({})", vec!["?"; items.len()].join(", "))
        }
        "is" => match val {
            "null" | "unknown" => format!("{c} IS NULL"),
            "true" => format!("{c} IS TRUE"),
            "false" => format!("{c} IS FALSE"),
            _ => return Err(bad()),
        },
        _ => return Err(ApiError::bad("PGRST100", format!("operator \"{op}\" is not supported by literest"))),
    };
    Ok(if negate { format!("NOT ({sql})") } else { sql })
}

/// LIKE pattern (PostgREST also accepts `*` for `%`) -> GLOB pattern.
fn like_to_glob(pattern: &str) -> String {
    pattern
        .chars()
        .map(|ch| match ch {
            '*' | '%' => "*".to_string(),
            '_' => "?".to_string(),
            '[' => "[[]".to_string(),
            ']' => "[]]".to_string(),
            '?' => "[?]".to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// `(a,"b,c",d)` -> ["a", "b,c", "d"]. Double quotes protect `,()`; `\` escapes inside quotes.
fn parse_list(raw: &str) -> Option<Vec<String>> {
    let inner = raw.strip_prefix('(')?.strip_suffix(')')?;
    if inner.is_empty() {
        return Some(vec![]);
    }
    let (mut out, mut cur, mut quoted) = (vec![], String::new(), false);
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => quoted = !quoted,
            '\\' if quoted => cur.extend(chars.next()),
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
    }
    out.push(cur);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_lists_respect_quotes() {
        assert_eq!(parse_list(r#"(1,"a,b",c)"#).unwrap(), ["1", "a,b", "c"]);
        assert_eq!(parse_list(r#"("say \"hi\"")"#).unwrap(), [r#"say "hi""#]);
        assert!(parse_list("()").unwrap().is_empty());
        assert!(parse_list("1,2").is_none());
    }

    #[test]
    fn like_becomes_case_sensitive_glob() {
        assert_eq!(like_to_glob("%a_b*[?]"), "*a?b*[[][?][]]");
    }
}
