// Nuage DB from Java 11+, with the standard library only (add Jackson or Gson to read the
// rows as objects).
//   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... java Nuage.java
//   Without NUAGE_TOKEN, with the server's NUAGE_ADMIN_TOKEN: it makes its own application first.
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;

public class Nuage {
    static final String URL = System.getenv().getOrDefault("NUAGE_URL", "http://localhost:8080");
    static String token = System.getenv("NUAGE_TOKEN");
    static final HttpClient HTTP = HttpClient.newHttpClient();

    /** Runs SQL with ? parameters (strings, numbers, booleans, null); returns the JSON answer. */
    static String sql(String query, Object... params) throws Exception {
        StringBuilder json = new StringBuilder("{\"sql\":").append(quote(query)).append(",\"params\":[");
        for (int i = 0; i < params.length; i++) {
            Object p = params[i];
            json.append(i > 0 ? "," : "").append(p instanceof String ? quote((String) p) : String.valueOf(p));
        }
        HttpRequest request = HttpRequest.newBuilder(URI.create(URL + "/v1/sql"))
                .header("Authorization", "Bearer " + token)
                .header("Content-Type", "application/json")
                .POST(HttpRequest.BodyPublishers.ofString(json.append("]}").toString()))
                .build();
        HttpResponse<String> res = HTTP.send(request, HttpResponse.BodyHandlers.ofString());
        if (res.statusCode() >= 400) throw new RuntimeException(res.body());
        return res.body();
    }

    /** Makes an application (a database of its own) with the management token; returns its token. */
    static String createApp(String name, String adminToken) throws Exception {
        HttpRequest request = HttpRequest.newBuilder(URI.create(URL + "/api/apps"))
                .header("Authorization", "Bearer " + adminToken)
                .header("Content-Type", "application/json")
                .POST(HttpRequest.BodyPublishers.ofString("{\"name\":" + quote(name) + "}"))
                .build();
        HttpResponse<String> res = HTTP.send(request, HttpResponse.BodyHandlers.ofString());
        if (res.statusCode() >= 400) throw new RuntimeException(res.body());
        java.util.regex.Matcher m = java.util.regex.Pattern.compile("\"token\":\"([^\"]+)\"").matcher(res.body());
        if (!m.find()) throw new RuntimeException("no token in " + res.body());
        return m.group(1);
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
        String admin = System.getenv("NUAGE_ADMIN_TOKEN");
        if ((token == null || token.isEmpty()) && admin != null && !admin.isEmpty()) {
            token = createApp("notes-java", admin);
            System.out.println("created the application notes-java");
        }
        sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)");
        System.out.println(sql("INSERT INTO notes (text) VALUES (?)", "hello from java"));
        System.out.println(sql("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5"));
    }
}
