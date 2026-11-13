// Nuage DB from C# (.NET 10: dotnet run csharp.cs; or paste into any .NET 6+ project).
//   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... dotnet run csharp.cs
using System.Net.Http.Headers;
using System.Net.Http.Json;
using System.Text.Json;

var url = Environment.GetEnvironmentVariable("NUAGE_URL") ?? "http://localhost:8080";
var http = new HttpClient();
http.DefaultRequestHeaders.Authorization =
    new AuthenticationHeaderValue("Bearer", Environment.GetEnvironmentVariable("NUAGE_TOKEN"));

// Runs SQL with ? parameters; returns {"columns", "rows"} or {"rows_affected", "last_insert_id"}.
async Task<JsonElement> Sql(string query, params object?[] parameters)
{
    var res = await http.PostAsJsonAsync($"{url}/v1/sql", new { sql = query, @params = parameters });
    var body = await res.Content.ReadFromJsonAsync<JsonElement>();
    if (body.TryGetProperty("error", out var error))
        throw new Exception($"{error.GetProperty("kind")}: {error.GetProperty("message")}");
    return body;
}

await Sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)");
var inserted = await Sql("INSERT INTO notes (text) VALUES (?)", "hello from csharp");
Console.WriteLine($"inserted {inserted.GetProperty("last_insert_id")}");
foreach (var row in (await Sql("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5")).GetProperty("rows").EnumerateArray())
    Console.WriteLine($"{row.GetProperty("id")} {row.GetProperty("text")}");
