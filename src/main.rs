//! literest: a PostgREST-compatible REST server on top of SQLite.

mod query;
mod schema;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use rusqlite::types::Value;
use rusqlite::{Connection, ffi, params_from_iter};
use serde_json::{Map, Value as Json, json};

use schema::{Column, quote};

// ponytail: one connection behind a global lock serializes every request.
// Fine for SQLite's single-writer model; add a read pool + WAL if reads pile up.
type Db = Arc<Mutex<Connection>>;

const USAGE: &str = "\
usage: literest [DB_PATH] [--port PORT] [--init SQL_FILE]

  DB_PATH   SQLite database file (default: literest.db; :memory: for a scratch db)
  --port    port to listen on (default: 3000)
  --init    SQL file executed at startup, e.g. CREATE TABLE IF NOT EXISTS ...

Tables are served at /<table> and /rest/v1/<table> (where supabase-js looks).";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let (mut db_path, mut port, mut init) = ("literest.db".to_string(), 3000u16, None);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => port = args.next().ok_or("--port needs a value")?.parse()?,
            "--init" => init = Some(args.next().ok_or("--init needs a file")?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => db_path = arg.clone(),
        }
    }

    let conn = Connection::open(&db_path)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    if let Some(file) = init {
        conn.execute_batch(&std::fs::read_to_string(file)?)?;
    }

    let app = Router::new()
        .route("/{table}", any(handle))
        .route("/rest/v1/{table}", any(handle))
        .with_state(Arc::new(Mutex::new(conn)));
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    println!(
        "literest {} serving {db_path} on http://{} (ready in {:.2?})",
        env!("CARGO_PKG_VERSION"),
        listener.local_addr()?,
        started.elapsed()
    );
    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle(
    State(db): State<Db>,
    method: Method,
    Path(table): Path<String>,
    Query(params): Query<Vec<(String, String)>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Reply, ApiError> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let req = Req {
        method,
        table,
        params,
        prefer: header("prefer"),
        single: header("accept").contains("application/vnd.pgrst.object+json"),
        body,
    };
    tokio::task::spawn_blocking(move || execute(&mut db.lock().unwrap_or_else(|e| e.into_inner()), req))
        .await
        .map_err(|e| ApiError::new(500, "XX000", e.to_string()))?
}

struct Req {
    method: Method,
    table: String,
    params: Vec<(String, String)>,
    prefer: String,
    /// `.single()` in supabase-js: answer with one object, not an array.
    single: bool,
    body: Bytes,
}

fn execute(conn: &mut Connection, req: Req) -> Result<Reply, ApiError> {
    let cols = schema::columns(conn, &req.table)?;
    let q = query::parse(conn, &cols, &req.params)?;
    let table = quote(&req.table);
    let returning = format!(" RETURNING {}", q.select_sql(&cols));
    let read = matches!(req.method, Method::GET | Method::HEAD);

    // Writes and `.single()` run in one transaction, so a failed `.single()`
    // check undoes the write, as PostgREST does.
    let tx = conn.transaction()?;
    let mut count = None;
    let rows = match req.method {
        Method::GET | Method::HEAD => {
            if req.prefer.contains("count=exact") {
                let sql = format!("SELECT COUNT(*) FROM {table}{}", q.where_sql);
                count = Some(tx.query_row(&sql, params_from_iter(&q.where_params), |r| r.get::<_, i64>(0))?);
            }
            let mut params = q.where_params.clone();
            params.extend([Value::Integer(q.limit.unwrap_or(-1)), Value::Integer(q.offset.unwrap_or(0))]);
            let sql = format!("SELECT {} FROM {table}{}{} LIMIT ? OFFSET ?", q.select_sql(&cols), q.where_sql, q.order_sql);
            fetch(&tx, &sql, &params, &cols, &q.select)?
        }
        Method::POST => insert(&tx, &req, &cols, &q, &returning)?,
        Method::PATCH => update(&tx, &req, &cols, &q, &returning)?,
        Method::DELETE => {
            let sql = format!("DELETE FROM {table}{}{returning}", q.where_sql);
            fetch(&tx, &sql, &q.where_params, &cols, &q.select)?
        }
        _ => return Err(ApiError::new(405, "PGRST117", format!("Unsupported HTTP method: {}", req.method))),
    };
    if req.single && rows.len() != 1 {
        return Err(ApiError::new(
            406,
            "PGRST116",
            format!("Cannot coerce the result to a single JSON object ({} rows)", rows.len()),
        ));
    }
    tx.commit()?;

    let repr = read || req.prefer.contains("return=representation");
    let offset = q.offset.unwrap_or(0);
    let status = match (&req.method, count) {
        (&Method::POST, _) => StatusCode::CREATED,
        // With a known total, PostgREST flags a partial page (206) or an offset past the end (416).
        (_, Some(total)) if read && offset > total => StatusCode::RANGE_NOT_SATISFIABLE,
        (_, Some(total)) if read && (rows.len() as i64) < total => StatusCode::PARTIAL_CONTENT,
        _ if repr => StatusCode::OK,
        _ => StatusCode::NO_CONTENT,
    };
    let content_range = read.then(|| {
        let total = count.map_or("*".to_string(), |c| c.to_string());
        match rows.len() as i64 {
            0 => format!("*/{total}"),
            n => format!("{offset}-{}/{total}", offset + n - 1),
        }
    });
    let body = repr.then(|| if req.single { rows.into_iter().next().unwrap_or(Json::Null) } else { Json::Array(rows) });
    Ok(Reply { status, body, single: req.single, content_range })
}

fn insert(tx: &Connection, req: &Req, cols: &[Column], q: &query::Query, returning: &str) -> Result<Vec<Json>, ApiError> {
    let rows: Vec<Map<String, Json>> = match json_body(&req.body)? {
        Json::Array(items) => items.into_iter().map(object).collect::<Result<_, _>>()?,
        one => vec![object(one)?],
    };
    // postgrest-js sends `columns` for bulk inserts; otherwise use every key seen.
    let keys = q.columns.clone().unwrap_or_else(|| {
        let mut keys: Vec<String> = vec![];
        for key in rows.iter().flat_map(|r| r.keys()) {
            if !keys.contains(key) {
                keys.push(key.clone());
            }
        }
        keys
    });
    let targets = keys.iter().map(|k| writable(cols, &req.table, k)).collect::<Result<Vec<_>, _>>()?;
    let missing_default = req.prefer.contains("missing=default");
    let table = quote(&req.table);

    let mut out = vec![];
    for row in &rows {
        let (mut names, mut params) = (vec![], vec![]);
        for col in &targets {
            params.push(match row.get(&col.name) {
                Some(v) => schema::from_json(tx, col.kind, v)?,
                None if missing_default => continue,
                None => Value::Null,
            });
            names.push(quote(&col.name));
        }
        let sql = if names.is_empty() {
            format!("INSERT INTO {table} DEFAULT VALUES{returning}")
        } else {
            let marks = vec!["?"; names.len()].join(", ");
            format!("INSERT INTO {table} ({}) VALUES ({marks}){returning}", names.join(", "))
        };
        out.extend(fetch(tx, &sql, &params, cols, &q.select)?);
    }
    Ok(out)
}

fn update(tx: &Connection, req: &Req, cols: &[Column], q: &query::Query, returning: &str) -> Result<Vec<Json>, ApiError> {
    let changes = object(json_body(&req.body)?)?;
    if changes.is_empty() {
        return Ok(vec![]);
    }
    let (mut sets, mut params) = (vec![], vec![]);
    for (key, v) in &changes {
        let col = writable(cols, &req.table, key)?;
        sets.push(format!("{} = ?", quote(&col.name)));
        params.push(schema::from_json(tx, col.kind, v)?);
    }
    params.extend(q.where_params.iter().cloned());
    let sql = format!("UPDATE {} SET {}{}{returning}", quote(&req.table), sets.join(", "), q.where_sql);
    fetch(tx, &sql, &params, cols, &q.select)
}

/// Runs `sql` and shapes each row as the JSON object PostgREST would return.
fn fetch(conn: &Connection, sql: &str, params: &[Value], cols: &[Column], select: &[(String, usize)]) -> Result<Vec<Json>, ApiError> {
    let mut stmt = conn.prepare_cached(sql)?;
    let mut rows = stmt.query(params_from_iter(params))?;
    let mut out = vec![];
    while let Some(row) = rows.next()? {
        let mut obj = Map::new();
        for (i, (key, ci)) in select.iter().enumerate() {
            obj.insert(key.clone(), schema::to_json(cols[*ci].kind, row.get_ref(i)?));
        }
        out.push(Json::Object(obj));
    }
    Ok(out)
}

fn writable<'a>(cols: &'a [Column], table: &str, key: &str) -> Result<&'a Column, ApiError> {
    query::find(cols, key)
        .map(|(_, c)| c)
        .map_err(|_| ApiError::bad("PGRST204", format!("Could not find the '{key}' column of '{table}' in the schema cache")))
}

fn json_body(body: &[u8]) -> Result<Json, ApiError> {
    serde_json::from_slice(body).map_err(|e| ApiError::bad("PGRST102", format!("Empty or invalid json: {e}")))
}

fn object(v: Json) -> Result<Map<String, Json>, ApiError> {
    match v {
        Json::Object(m) => Ok(m),
        _ => Err(ApiError::bad("PGRST102", "expected a JSON object or an array of objects")),
    }
}

#[derive(Debug)]
struct Reply {
    status: StatusCode,
    body: Option<Json>,
    single: bool,
    content_range: Option<String>,
}

impl IntoResponse for Reply {
    fn into_response(self) -> Response {
        let mut res = match self.body {
            Some(body) => {
                let ct = if self.single {
                    "application/vnd.pgrst.object+json; charset=utf-8"
                } else {
                    "application/json; charset=utf-8"
                };
                (self.status, [(header::CONTENT_TYPE, ct)], body.to_string()).into_response()
            }
            None => self.status.into_response(),
        };
        if let Some(Ok(v)) = self.content_range.map(HeaderValue::try_from) {
            res.headers_mut().insert(header::CONTENT_RANGE, v);
        }
        res
    }
}

/// Errors in PostgREST's JSON shape. `code` is the Postgres SQLSTATE (or
/// PGRSTxxx) the same failure produces there, so clients can branch on it.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        Self { status, code, message: message.into() }
    }

    pub fn bad(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(400, code, message)
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(f, msg) = &e {
            let (status, code) = match f.extended_code {
                ffi::SQLITE_CONSTRAINT_UNIQUE | ffi::SQLITE_CONSTRAINT_PRIMARYKEY => (409, "23505"),
                ffi::SQLITE_CONSTRAINT_FOREIGNKEY => (409, "23503"),
                ffi::SQLITE_CONSTRAINT_NOTNULL => (400, "23502"),
                ffi::SQLITE_CONSTRAINT_CHECK => (400, "23514"),
                _ => (500, "XX000"),
            };
            return Self::new(status, code, msg.clone().unwrap_or_else(|| e.to_string()));
        }
        Self::new(500, "XX000", e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "code": self.code, "details": null, "hint": null, "message": self.message });
        (self.status, axum::Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL, done BOOLEAN NOT NULL DEFAULT FALSE,
                             meta JSONB, at TIMESTAMPTZ, n INTEGER)",
        )
        .unwrap();
        conn
    }

    /// `query` is already URL-decoded: `k=v&k=v`.
    fn run(conn: &mut Connection, method: Method, query: &str, prefer: &str, body: &str) -> Result<Reply, ApiError> {
        let params = query
            .split('&')
            .filter(|s| !s.is_empty())
            .map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).unwrap())
            .collect();
        let (prefer, body) = (prefer.to_string(), Bytes::from(body.to_string()));
        execute(conn, Req { method, table: "t".into(), params, prefer, single: false, body })
    }

    fn rows(conn: &mut Connection, query: &str) -> Json {
        run(conn, Method::GET, query, "", "").unwrap().body.unwrap()
    }

    #[test]
    fn end_to_end_over_sqlite() {
        let c = &mut db();

        // Booleans, JSON and timestamps go in as Postgres types and come back the way PostgREST shows them.
        let body = r#"{"id":1,"name":"a","done":true,"meta":{"k":[1,"x"]},"at":"2024-01-15T12:30:00.5+02:00"}"#;
        let r = run(c, Method::POST, "", "return=representation", body).unwrap();
        assert_eq!(r.status, StatusCode::CREATED);
        assert_eq!(
            r.body.unwrap(),
            json!([{"id":1,"name":"a","done":true,"meta":{"k":[1,"x"]},"at":"2024-01-15T10:30:00.5+00:00","n":null}])
        );
        let r = run(c, Method::POST, "", "", r#"{"id":2,"name":"b","n":5}"#).unwrap();
        assert_eq!((r.status, r.body), (StatusCode::CREATED, None));

        assert_eq!(rows(c, "select=id,done&done=eq.false"), json!([{"id":2,"done":false}]));
        assert_eq!(rows(c, "select=id&done=is.true"), json!([{"id":1}]));
        assert_eq!(rows(c, "select=key:id&at=gt.2024-01-15T10:00:00Z"), json!([{"key":1}]));

        // Postgres NULL ordering: last ascending, first descending.
        assert_eq!(rows(c, "select=id&order=n.asc"), json!([{"id":2},{"id":1}]));
        assert_eq!(rows(c, "select=id&order=n.desc"), json!([{"id":1},{"id":2}]));
        assert_eq!(rows(c, "select=id&order=n.asc.nullsfirst"), json!([{"id":1},{"id":2}]));

        assert_eq!(rows(c, "select=id&id=in.(1,2)&order=id.desc&limit=1&offset=1"), json!([{"id":1}]));
        assert_eq!(rows(c, "select=id&name=like.A"), json!([]));
        assert_eq!(rows(c, "select=id&name=ilike.A"), json!([{"id":1}]));
        assert_eq!(rows(c, "select=id&n=not.is.null"), json!([{"id":2}]));

        // Values are bound, never spliced.
        assert_eq!(rows(c, "name=eq.a' OR '1'='1"), json!([]));

        let r = run(c, Method::PATCH, "id=eq.2&select=id,done", "return=representation", r#"{"done":true}"#).unwrap();
        assert_eq!(r.body.unwrap(), json!([{"id":2,"done":true}]));
        let r = run(c, Method::DELETE, "id=eq.1", "", "").unwrap();
        assert_eq!((r.status, r.body), (StatusCode::NO_CONTENT, None));
        assert_eq!(rows(c, "select=id"), json!([{"id":2}]));

        let status = |r: Result<Reply, ApiError>| r.unwrap_err().status.as_u16();
        assert_eq!(status(run(c, Method::GET, "select=nope", "", "")), 400);
        assert_eq!(status(run(c, Method::GET, "id=near.1", "", "")), 400);
        assert_eq!(status(run(c, Method::POST, "", "", r#"{"id":2,"name":"dup"}"#)), 409);
        assert_eq!(status(run(c, Method::POST, "", "", r#"{"id":3}"#)), 400);
    }
}
