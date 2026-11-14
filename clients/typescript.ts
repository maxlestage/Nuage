// Nuage DB from TypeScript (Bun, Deno, Node 22+ with --experimental-strip-types, or any bundler).
//   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... bun typescript.ts
//   Without NUAGE_TOKEN, with the server's NUAGE_ADMIN_TOKEN: it makes its own application first.
const URL = process.env.NUAGE_URL ?? "http://localhost:8080";
let TOKEN = process.env.NUAGE_TOKEN;

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

/** Makes an application (a database of its own) with the management token; returns its token. */
async function createApp(name: string, adminToken: string): Promise<string> {
  const res = await fetch(`${URL}/api/apps`, {
    method: "POST",
    headers: { Authorization: `Bearer ${adminToken}`, "Content-Type": "application/json" },
    body: JSON.stringify({ name }),
  });
  const body = await res.json();
  if (!res.ok) throw new Error(`${body.error.kind}: ${body.error.message}`);
  return body.token;
}

interface Note {
  id: number;
  text: string;
}

if (!TOKEN && process.env.NUAGE_ADMIN_TOKEN) {
  TOKEN = await createApp("notes-typescript", process.env.NUAGE_ADMIN_TOKEN);
  console.log("created the application notes-typescript");
}

await sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)");
await sql("INSERT INTO notes (text) VALUES (?)", ["hello from typescript"]);
const { rows = [] } = await sql<Note>("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5");
for (const note of rows) console.log(note.id, note.text);
