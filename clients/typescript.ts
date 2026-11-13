// Nuage DB from TypeScript (Bun, Deno, Node 22+ with --experimental-strip-types, or any bundler).
//   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... bun typescript.ts
const URL = process.env.NUAGE_URL ?? "http://localhost:8080";
const TOKEN = process.env.NUAGE_TOKEN;

type Value = string | number | boolean | null | Value[] | { [key: string]: Value };
interface Result<Row = Record<string, Value>> {
  columns?: string[];
  rows?: Row[];
  rows_affected?: number;
  last_insert_id?: Value;
}

async function sql<Row = Record<string, Value>>(query: string, params: Value[] = []): Promise<Result<Row>> {
  const res = await fetch(`${URL}/v1/sql`, {
    method: "POST",
    headers: { Authorization: `Bearer ${TOKEN}`, "Content-Type": "application/json" },
    body: JSON.stringify({ sql: query, params }),
  });
  const body = await res.json();
  if (!res.ok) throw new Error(`${body.error.kind}: ${body.error.message}`);
  return body;
}

interface Note {
  id: number;
  text: string;
}

await sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)");
await sql("INSERT INTO notes (text) VALUES (?)", ["hello from typescript"]);
const { rows = [] } = await sql<Note>("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5");
for (const note of rows) console.log(note.id, note.text);
