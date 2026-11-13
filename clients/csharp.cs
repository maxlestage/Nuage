// Nuage DB from C# (.NET 6+; .NET 10 runs this file directly: dotnet run csharp.cs).
// JsonNode needs no reflection: it works with trimming and AOT too.
//   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... dotnet run csharp.cs
using System.Net.Http.Headers;
using System.Text;
using System.Text.Json.Nodes;

var url = Environment.GetEnvironmentVariable("NUAGE_URL") ?? "http://localhost:8080";
var http = new HttpClient();
http.DefaultRequestHeaders.Authorization =
    new AuthenticationHeaderValue("Bearer", Environment.GetEnvironmentVariable("NUAGE_TOKEN"));

// Runs SQL with ? parameters; returns {"columns", "rows"} or {"rows_affected", "last_insert_id"}.
async Task<JsonNode> Sql(string query, params JsonNode?[] parameters)
{
    var request = new JsonObject { ["sql"] = query, ["params"] = new JsonArray(parameters) };
    var content = new StringContent(request.ToJsonString(), Encoding.UTF8, "application/json");
    var res = await http.PostAsync($"{url}/v1/sql", content);
    var body = JsonNode.Parse(await res.Content.ReadAsStringAsync())!;
    if (body["error"] is JsonNode error) throw new Exception($"{error["kind"]}: {error["message"]}");
    return body;
}

await Sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)");
var inserted = await Sql("INSERT INTO notes (text) VALUES (?)", "hello from csharp");
Console.WriteLine($"inserted {inserted["last_insert_id"]}");
foreach (var row in (await Sql("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5"))["rows"]!.AsArray())
    Console.WriteLine($"{row!["id"]} {row["text"]}");
