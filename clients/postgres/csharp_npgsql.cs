// Nuage DB with Npgsql, the .NET PostgreSQL driver (Entity Framework Core and Dapper use it).
// The password is a token of an application. "Server Compatibility Mode=NoTypeLoading": Nuage
// DB has no pg_catalog for Npgsql to read types from; the built-in types are all there.
//   NUAGE_TOKEN=... dotnet run csharp_npgsql.cs   (.NET 10; or `dotnet add package Npgsql`)
#:package Npgsql@9.0.3
using Npgsql;

var host = Environment.GetEnvironmentVariable("NUAGE_PG_HOST") ?? "localhost";
var port = Environment.GetEnvironmentVariable("NUAGE_PG_PORT") ?? "5432";
var token = Environment.GetEnvironmentVariable("NUAGE_TOKEN");
await using var db = NpgsqlDataSource.Create(
    $"Host={host};Port={port};Username=app;Password={token};Database=nuage;Server Compatibility Mode=NoTypeLoading");

await using (var create = db.CreateCommand("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)"))
    await create.ExecuteNonQueryAsync();

await using var conn = await db.OpenConnectionAsync();
await using var tx = await conn.BeginTransactionAsync();
await using var insert = new NpgsqlCommand("INSERT INTO notes (text) VALUES ($1) RETURNING id", conn, tx);
insert.Parameters.AddWithValue("hello from npgsql");
var id = (long)(await insert.ExecuteScalarAsync())!;
await tx.CommitAsync();
Console.WriteLine($"inserted {id}");

await using var select = new NpgsqlCommand("SELECT id, text FROM notes WHERE id <= $1 ORDER BY id DESC LIMIT 5", conn);
select.Parameters.AddWithValue(id);
await using var rows = await select.ExecuteReaderAsync();
while (await rows.ReadAsync())
    Console.WriteLine($"{rows.GetInt64(0)} {rows.GetString(1)}");
