// Nuage DB with node-postgres (npm install pg); Prisma, Knex, Sequelize and TypeORM use it.
// The password is a token of an application.
//   NUAGE_TOKEN=... node node_pg.mjs
import pg from "pg";

const client = new pg.Client({
  host: process.env.NUAGE_PG_HOST ?? "localhost",
  port: Number(process.env.NUAGE_PG_PORT ?? 5432),
  user: "app",
  database: "nuage",
  password: process.env.NUAGE_TOKEN,
});
await client.connect();
await client.query("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)");
await client.query("BEGIN");
const { rows: [inserted] } = await client.query("INSERT INTO notes (text) VALUES ($1) RETURNING id", ["hello from node-postgres"]);
await client.query("COMMIT");
console.log("inserted", inserted.id);
const { rows } = await client.query("SELECT id, text FROM notes WHERE id <= $1 ORDER BY id DESC LIMIT 5", [inserted.id]);
for (const row of rows) console.log(row.id, row.text);
await client.end();
