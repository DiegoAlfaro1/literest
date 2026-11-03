-- Loaded into both Postgres (via docker-entrypoint-initdb.d) and literest (via --init).
-- The same DDL is valid in both; literest reads the declared types to know which
-- columns are booleans, timestamps and JSON.
create table if not exists todos (
  id integer primary key,
  title text not null,
  done boolean not null default false,
  priority integer,
  score double precision,
  meta jsonb,
  created_at timestamptz,
  due date
);
