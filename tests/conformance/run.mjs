// Conformance: send the same supabase-js queries to real PostgREST (+ Postgres)
// and to literest, and diff what the client sees.
//
// PostgrestClient is the query builder behind `supabase.from(...)`. PostgREST
// serves tables at the root, so both servers are driven through it directly;
// one extra check goes through createClient() against literest's /rest/v1.
import { PostgrestClient } from '@supabase/postgrest-js'
import { createClient } from '@supabase/supabase-js'
import { inspect, isDeepStrictEqual } from 'node:util'

const POSTGREST = process.env.POSTGREST_URL ?? 'http://localhost:3000'
const LITEREST = process.env.LITEREST_URL ?? 'http://localhost:3001'
const pg = new PostgrestClient(POSTGREST)
const lite = new PostgrestClient(LITEREST)

const seed = [
  { id: 1, title: 'write code', done: true, priority: 1, score: 9.5, meta: { owner: 'diego', tags: ['rust', 'sqlite'] }, created_at: '2024-01-15T10:30:00Z', due: '2024-02-01' },
  { id: 2, title: 'Write docs', done: false, priority: null, score: 7.25, meta: [], created_at: '2024-01-16T08:00:00.5+02:00', due: null },
  { id: 3, title: 'ship it', done: false, priority: 3, score: null, meta: { nested: { deep: true } }, created_at: null, due: '2024-03-01' },
  { id: 4, title: 'celebrate', done: true, priority: 2, score: 1, meta: null, created_at: '2023-12-31T23:59:59Z', due: '2024-01-01' },
  { id: 5, title: 'Comma, (parens)', done: false, priority: null, score: 0, meta: 'just a string', created_at: '2024-06-01T00:00:00-05:00', due: null },
]

const todos = (c) => c.from('todos')

// Reads first, against the seed. Writes after, in order: each sees the ones before it.
const cases = [
  ['select *', (c) => todos(c).select('*').order('id')],
  ['select columns', (c) => todos(c).select('id,title').order('id')],
  ['select with alias', (c) => todos(c).select('id, name:title').order('id')],

  ['eq', (c) => todos(c).select('id').eq('priority', 3)],
  ['neq', (c) => todos(c).select('id').neq('title', 'ship it').order('id')],
  ['gt', (c) => todos(c).select('id').gt('score', 5).order('id')],
  ['gte', (c) => todos(c).select('id').gte('score', 7.25).order('id')],
  ['lt', (c) => todos(c).select('id').lt('priority', 3).order('id')],
  ['lte', (c) => todos(c).select('id').lte('priority', 2).order('id')],
  ['in', (c) => todos(c).select('id').in('id', [1, 3, 5]).order('id')],
  ['in with quoted values', (c) => todos(c).select('id').in('title', ['Comma, (parens)', 'ship it']).order('id')],
  ['in empty list', (c) => todos(c).select('id').in('id', [])],
  ['is null', (c) => todos(c).select('id').is('priority', null).order('id')],
  ['not is null', (c) => todos(c).select('id').not('priority', 'is', null).order('id')],
  ['multiple filters', (c) => todos(c).select('id').gt('id', 1).lt('id', 5).order('id')],
  ['like is case-sensitive', (c) => todos(c).select('id').like('title', '%Write%')],
  ['ilike', (c) => todos(c).select('id').ilike('title', 'write%').order('id')],

  ['boolean eq true', (c) => todos(c).select('id, done').eq('done', true).order('id')],
  ['boolean eq false', (c) => todos(c).select('id').eq('done', false).order('id')],
  ['boolean is true', (c) => todos(c).select('id').is('done', true).order('id')],
  ['boolean is false', (c) => todos(c).select('id').is('done', false).order('id')],
  ['timestamptz output', (c) => todos(c).select('id, created_at').order('id')],
  ['timestamptz filter with offset', (c) => todos(c).select('id').gt('created_at', '2024-01-15T12:00:00+02:00').order('id')],
  ['date eq', (c) => todos(c).select('id').eq('due', '2024-03-01')],
  ['date range', (c) => todos(c).select('id').gte('due', '2024-01-15').lt('due', '2024-03-01').order('id')],
  ['json output', (c) => todos(c).select('id, meta').order('id')],

  ['order nulls default asc', (c) => todos(c).select('id, priority').order('priority').order('id')],
  ['order nulls default desc', (c) => todos(c).select('id, priority').order('priority', { ascending: false }).order('id')],
  ['order nullsFirst asc', (c) => todos(c).select('id').order('priority', { nullsFirst: true }).order('id')],
  ['order nullsLast desc', (c) => todos(c).select('id').order('priority', { ascending: false, nullsFirst: false }).order('id')],
  ['order text (C collation)', (c) => todos(c).select('title').order('title')],
  ['order boolean', (c) => todos(c).select('id').order('done').order('id')],
  ['order timestamptz', (c) => todos(c).select('id').order('created_at', { ascending: false })],
  ['range', (c) => todos(c).select('id').order('id').range(1, 3)],
  ['range past the end', (c) => todos(c).select('id').order('id').range(10, 20)],
  ['limit', (c) => todos(c).select('id').order('id').limit(2)],
  ['count exact', (c) => todos(c).select('*', { count: 'exact' }).order('id').range(0, 1)],
  ['count exact, range past the end', (c) => todos(c).select('id', { count: 'exact' }).order('id').range(10, 20)],
  ['count exact head',(c) => todos(c).select('*', { count: 'exact', head: true }).eq('done', false)],
  ['single', (c) => todos(c).select('id, title').eq('id', 1).single()],
  ['single with no rows', (c) => todos(c).select('id').eq('id', 999).single()],
  ['single with many rows', (c) => todos(c).select('id').single()],
  ['unknown column', (c) => todos(c).select('nope')],
  ['unknown filter column', (c) => todos(c).select('id').eq('nope', 1)],
  ['unknown table', (c) => c.from('nope').select('*')],

  ['insert returning', (c) => todos(c).insert({ id: 10, title: 'new', done: true, meta: { a: [1, 'two', null] }, created_at: '2024-05-05T05:05:05.25-05:00', due: '2024-05-05' }).select()],
  ['bulk insert, minimal', (c) => todos(c).insert([{ id: 11, title: 'a' }, { id: 12, title: 'b', priority: 5 }])],
  ['insert uses column defaults', (c) => todos(c).insert({ id: 13, title: 'defaults' }).select('id, done, meta')],
  ['insert duplicate key', (c) => todos(c).insert({ id: 1, title: 'dup' })],
  ['insert null into not-null', (c) => todos(c).insert({ id: 14, title: null })],
  ['insert unknown column', (c) => todos(c).insert({ id: 15, title: 'x', nope: 1 })],
  ['update returning', (c) => todos(c).update({ done: true, priority: 9, meta: ['x'] }).eq('id', 2).select()],
  ['update minimal', (c) => todos(c).update({ title: 'renamed' }).eq('id', 3)],
  ['update no match', (c) => todos(c).update({ title: 'nobody' }).eq('id', 999).select()],
  ['delete returning', (c) => todos(c).delete().in('id', [11, 12]).select('id, title')],
  ['delete minimal', (c) => todos(c).delete().eq('id', 13)],
  ['state after writes', (c) => todos(c).select('*').order('id')],
]

// Everything a supabase-js caller can observe except error wording, which
// necessarily differs between Postgres and SQLite.
const observable = ({ status, data, count, error }) => ({ status, data, count, failed: error !== null })

async function waitUntilUp(url) {
  for (let i = 0; i < 60; i++) {
    try {
      if ((await fetch(`${url}/todos?limit=0`)).ok) return
    } catch {}
    await new Promise((r) => setTimeout(r, 500))
  }
  throw new Error(`${url} did not come up`)
}

await Promise.all([waitUntilUp(POSTGREST), waitUntilUp(LITEREST)])
for (const c of [pg, lite]) {
  const cleared = await todos(c).delete().gte('id', 0)
  const seeded = await todos(c).insert(seed)
  if (cleared.error || seeded.error) throw new Error(inspect({ cleared, seeded }))
}

let failures = 0
const check = (name, expected, actual) => {
  if (isDeepStrictEqual(observable(expected), observable(actual))) {
    console.log(`ok    ${name}`)
    return
  }
  failures++
  const show = (r) => inspect({ ...observable(r), error: r.error }, { depth: null, breakLength: 120 })
  console.log(`FAIL  ${name}\n  postgrest: ${show(expected)}\n  literest:  ${show(actual)}`)
}

for (const [name, build] of cases) {
  const expected = await build(pg)
  const actual = await build(lite)
  check(name, expected, actual)
}

// Unchanged supabase-js code: createClient() talks to literest's /rest/v1.
const supabase = createClient(LITEREST, 'any-anon-key', { auth: { persistSession: false, autoRefreshToken: false } })
check(
  'supabase-js createClient',
  await todos(pg).select('id, title, done').order('id'),
  await supabase.from('todos').select('id, title, done').order('id'),
)

console.log(`\n${cases.length + 1 - failures}/${cases.length + 1} cases match PostgREST`)
process.exit(failures ? 1 : 0)
