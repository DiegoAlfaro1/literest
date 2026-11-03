# literest

A [PostgREST](https://postgrest.org)-compatible REST server on top of SQLite, written in Rust
(axum + tokio + rusqlite). Point `supabase-js` at it and the same code runs unchanged.
One binary, one SQLite file.

```js
import { createClient } from '@supabase/supabase-js'

const supabase = createClient('http://localhost:3000', 'any-key')
const { data } = await supabase
  .from('todos')
  .select('id, title, done')
  .eq('done', false)
  .order('created_at', { ascending: false })
  .range(0, 9)
```

**Release binary: 2.9 MB, SQLite included · ready to accept connections in ~3 ms** (Windows x64;
CI reports the Linux numbers in each run's summary). **58/58 conformance cases match real PostgREST.**

## Run it

```sh
cargo build --release
./target/release/literest app.db --port 3000 --init schema.sql
```

```
usage: literest [DB_PATH] [--port PORT] [--init SQL_FILE]

  DB_PATH   SQLite database file (default: literest.db; :memory: for a scratch db)
  --port    port to listen on (default: 3000)
  --init    SQL file executed at startup, e.g. CREATE TABLE IF NOT EXISTS ...
```

Tables are served at `/<table>` (like PostgREST) and `/rest/v1/<table>` (where `createClient`
sends requests). Write your schema with Postgres type names; the declared types are how literest
knows a column is a boolean, a timestamp or JSON:

```sql
create table if not exists todos (
  id integer primary key,
  title text not null,
  done boolean not null default false,
  meta jsonb,
  created_at timestamptz
);
```

## Supported

| supabase-js | PostgREST request | Notes |
|---|---|---|
| `.select('a, b')`, `.select('*')`, `.select('alias:col')` | `select=` | |
| `.eq` `.neq` `.gt` `.gte` `.lt` `.lte` | `col=eq.v` ... | |
| `.in('col', [...])` | `col=in.(a,"b,c")` | quoted values with `,()` work |
| `.is('col', null \| true \| false)` | `col=is.null` | |
| `.like` / `.ilike` | `col=like.*x*` | `like` is case-sensitive, as in Postgres |
| `.not('col', op, v)` | `col=not.op.v` | |
| `.order('col', { ascending, nullsFirst })` | `order=col.desc.nullslast` | multiple columns |
| `.range(from, to)`, `.limit(n)` | `offset=` / `limit=` | |
| `.select('*', { count: 'exact', head })` | `Prefer: count=exact` | `Content-Range` header; 206 for a partial page, 416 past the end |
| `.single()` | `Accept: application/vnd.pgrst.object+json` | 406 unless exactly one row; writes are rolled back |
| `.insert(obj \| [objs])` | `POST`, `columns=` | `defaultToNull: false` (`missing=default`) supported |
| `.update(obj)` | `PATCH` | |
| `.delete()` | `DELETE` | |
| `.insert/.update/.delete(...).select(...)` | `Prefer: return=representation` | via SQLite `RETURNING` |

Errors come back in PostgREST's shape (`{ code, message, details, hint }`) with the Postgres
SQLSTATE where one applies: `23505` unique violation (409), `23502` not-null (400), `23503`
foreign key (409), `42703` unknown column (400), `PGRST116` for `.single()` (406), `PGRST205`
unknown table (404).

## Postgres vs. SQLite differences, and how they're handled

| | Postgres | SQLite | literest |
|---|---|---|---|
| Booleans | `boolean` | stored as `0`/`1` | `boolean` columns are returned as `true`/`false`; filters accept `true/false/t/f/...` |
| Timestamps | `timestamptz` | no type; text | Input parsed (ISO-8601, `Z` or `±HH:MM`), converted to UTC and stored as `2024-01-15T10:30:00.5+00:00`, exactly how PostgREST prints it, so text order is time order |
| Dates | `date` | text | Normalized to `YYYY-MM-DD` |
| JSON | `json` / `jsonb` | text | Written as serialized JSON, returned as parsed JSON (not a string) |
| NULL ordering | NULL is largest: last on `ASC`, first on `DESC` | NULL is smallest | Every `ORDER BY` spells out `NULLS FIRST/LAST` to match Postgres |
| `like` | case-sensitive | `LIKE` ignores ASCII case | `like` → `GLOB`, `ilike` → `LIKE` |

Table and column names from the URL are only used after matching the table's real schema
(`pragma_table_info`), and every value is a bound `?` parameter. No SQL is built from request
values.

## Not supported (yet)

Being explicit about the gaps:

- **Resource embedding** (`select('*, author(name)')`), i.e. joins through foreign keys. Returns 400.
- **`or` / `and` logic trees** (`.or('a.eq.1,b.eq.2')`). Returns 400.
- **Upsert** (`.upsert()`, `Prefer: resolution=...`, `on_conflict`).
- **JSON path operators** (`meta->key`, `meta->>key`) and **casts** (`col::text`) in `select` and filters.
- Other filter operators: `match`, `imatch`, full-text search (`fts`, `plfts`, ...), array/range
  operators (`cs`, `cd`, `ov`, ...), `isdistinct`, `like(any)`.
- RPC (`/rpc/fn`), views with `INSTEAD OF` triggers, OpenAPI at `/`.
- Auth: no JWT verification, roles or row-level security. Every request has full access. Don't
  expose it publicly as-is.
- `Range` request header, `count: 'planned' | 'estimated'`, CSV output, `Prefer: tx=rollback`,
  `limit` on `update`/`delete`.
- CORS headers (so browser apps on another origin can't call it yet).
- Locale-aware text ordering: SQLite compares bytes, like Postgres's `C` collation.
- Timestamps keep millisecond precision (Postgres keeps microseconds). `timestamp` (no time zone)
  columns convert offsets to UTC, while Postgres drops them.
- One SQLite connection behind a lock. Requests are serialized, which suits SQLite's single
  writer but leaves read concurrency on the table.

## Conformance tests

[`tests/conformance/run.mjs`](tests/conformance/run.mjs) runs the same supabase-js queries against
real PostgREST + Postgres (in Docker) and against literest, then diffs everything the client can
see: `status`, `data`, `count`, and whether `error` is set. (Error *messages* differ between the
engines, so they're printed but not compared.) It covers every row of the tables above, plus the
error cases. CI runs it on every push.

```sh
docker compose -f tests/conformance/docker-compose.yml up -d
cargo run --release -- :memory: --port 3001 --init tests/conformance/schema.sql &
cd tests/conformance && npm ci && node run.mjs
```

Unit tests: `cargo test`.

## Measuring

The server prints its own startup time (`ready in ...`, measured from `main` to the socket
listening), and CI writes the release binary size and that line to the job summary.

```sh
cargo build --release
ls -l target/release/literest        # binary size
./target/release/literest :memory:   # prints "ready in ..."
```

## License

MIT
