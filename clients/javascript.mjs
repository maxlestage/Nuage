// Nuage DB from JavaScript: Node 18+, Deno, Bun, and the browser (list the website in the
// console, Settings → Websites, and use a read-only token in pages).
//   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... node javascript.mjs
const URL = process.env.NUAGE_URL ?? "http://localhost:8080";
const TOKEN = process.env.NUAGE_TOKEN;

/** Runs SQL with `?` parameters; returns `{ columns, rows }` or `{ rows_affected, last_insert_id }`. */
async function sql(query, params = []) {
  const res = await fetch(`${URL}/v1/sql`, {
    method: "POST",
    headers: { Authorization: `Bearer ${TOKEN}`, "Content-Type": "application/json" },
    body: JSON.stringify({ sql: query, params }),
  });
  const body = await res.json();
  if (!res.ok) throw new Error(`${body.error.kind}: ${body.error.message}`);
  return body;
}

await sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)");
const { last_insert_id } = await sql("INSERT INTO notes (text) VALUES (?)", ["hello from javascript"]);
console.log("inserted", last_insert_id);
const { rows } = await sql("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5");
for (const row of rows) console.log(row.id, row.text);
