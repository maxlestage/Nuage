// Nuage DB from Java 11+, with the standard library only (add Jackson or Gson to read the
// rows as objects).
//   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... java Nuage.java
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;

public class Nuage {
    static final String URL = System.getenv().getOrDefault("NUAGE_URL", "http://localhost:8080");
    static final String TOKEN = System.getenv("NUAGE_TOKEN");
    static final HttpClient HTTP = HttpClient.newHttpClient();

    /** Runs SQL with ? parameters (strings, numbers, booleans, null); returns the JSON answer. */
    static String sql(String query, Object... params) throws Exception {
        StringBuilder json = new StringBuilder("{\"sql\":").append(quote(query)).append(",\"params\":[");
        for (int i = 0; i < params.length; i++) {
            Object p = params[i];
            json.append(i > 0 ? "," : "").append(p instanceof String ? quote((String) p) : String.valueOf(p));
        }
        HttpRequest request = HttpRequest.newBuilder(URI.create(URL + "/v1/sql"))
                .header("Authorization", "Bearer " + TOKEN)
                .header("Content-Type", "application/json")
                .POST(HttpRequest.BodyPublishers.ofString(json.append("]}").toString()))
                .build();
        HttpResponse<String> res = HTTP.send(request, HttpResponse.BodyHandlers.ofString());
        if (res.statusCode() >= 400) throw new RuntimeException(res.body());
        return res.body();
    }

    static String quote(String s) {
        StringBuilder out = new StringBuilder("\"");
        for (char c : s.toCharArray()) {
            if (c == '"' || c == '\\') out.append('\\').append(c);
            else if (c < 0x20) out.append(String.format("\\u%04x", (int) c));
            else out.append(c);
        }
        return out.append('"').toString();
    }

    public static void main(String[] args) throws Exception {
        sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)");
        System.out.println(sql("INSERT INTO notes (text) VALUES (?)", "hello from java"));
        System.out.println(sql("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5"));
    }
}
