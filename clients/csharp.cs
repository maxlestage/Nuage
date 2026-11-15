// Nuage DB from C# (.NET 6+; .NET 10 runs this file directly: dotnet run csharp.cs).
// JsonNode needs no reflection: it works with trimming and AOT too.
//   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... dotnet run csharp.cs
//   Without NUAGE_TOKEN, with the server's NUAGE_ADMIN_TOKEN: it makes its own application first.
using System.Net.Http.Headers;
using System.Text;
using System.Text.Json.Nodes;

var url = Environment.GetEnvironmentVariable("NUAGE_URL") ?? "http://localhost:8080";
var http = new HttpClient();
var token = Environment.GetEnvironmentVariable("NUAGE_TOKEN");
var admin = Environment.GetEnvironmentVariable("NUAGE_ADMIN_TOKEN");

// Makes an application (a database of its own) with the management token; returns its token.
async Task<string> CreateApp(string name, string adminToken)
{
    var json = new JsonObject { ["name"] = name }.ToJsonString();
    using var request = new HttpRequestMessage(HttpMethod.Post, $"{url}/api/apps")
    {
        Content = new StringContent(json, Encoding.UTF8, "application/json"),
    };
    request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", adminToken);
    var res = await http.SendAsync(request);
    var body = JsonNode.Parse(await res.Content.ReadAsStringAsync())!;
    if (body["error"] is JsonNode error) throw new Exception($"{error["kind"]}: {error["message"]}");
    return body["token"]!.GetValue<string>();
}

if (string.IsNullOrEmpty(token) && !string.IsNullOrEmpty(admin))
{
    token = await CreateApp("notes-csharp", admin);
    Console.WriteLine("created the application notes-csharp");
}
http.DefaultRequestHeaders.Authorization = new AuthenticationHeaderValue("Bearer", token);

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
