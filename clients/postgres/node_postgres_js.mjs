// Nuage DB with Postgres.js (npm install postgres). The password is a token.
//   NUAGE_TOKEN=... node node_postgres_js.mjs
import postgres from "postgres";

const sql = postgres({
  host: process.env.NUAGE_PG_HOST ?? "localhost",
  port: Number(process.env.NUAGE_PG_PORT ?? 5432),
  username: "app",
  database: "nuage",
  password: process.env.NUAGE_TOKEN,
  fetch_types: false, // Nuage DB has no pg_catalog to read types from
});
await sql`CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)`;
const [inserted] = await sql.begin((tx) => tx`INSERT INTO notes (text) VALUES (${"hello from postgres.js"}) RETURNING id`);
console.log("inserted", inserted.id);
for (const row of await sql`SELECT id, text FROM notes ORDER BY id DESC LIMIT ${5}`) console.log(row.id, row.text);
await sql.end();
