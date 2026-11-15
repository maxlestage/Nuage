// Nuage DB from Kotlin (JVM, Android with any HTTP client), with the JDK only (add
// kotlinx.serialization, Moshi or Gson to read the rows as objects).
//   kotlinc kotlin.kt -include-runtime -d nuage.jar && NUAGE_TOKEN=... java -jar nuage.jar
//   Without NUAGE_TOKEN, with the server's NUAGE_ADMIN_TOKEN: it makes its own application first.
import java.net.URI
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse

val url = System.getenv("NUAGE_URL") ?: "http://localhost:8080"
var token: String = System.getenv("NUAGE_TOKEN") ?: ""
val http: HttpClient = HttpClient.newHttpClient()

fun quote(s: String) = buildString {
    append('"')
    for (c in s) when {
        c == '"' || c == '\\' -> append('\\').append(c)
        c < ' ' -> append("\\u%04x".format(c.code))
        else -> append(c)
    }
    append('"')
}

/** Runs SQL with ? parameters (strings, numbers, booleans, null); returns the JSON answer. */
fun sql(query: String, vararg params: Any?): String {
    val json = """{"sql": ${quote(query)}, "params": [${params.joinToString(",") { if (it is String) quote(it) else it.toString() }}]}"""
    val request = HttpRequest.newBuilder(URI.create("$url/v1/sql"))
        .header("Authorization", "Bearer $token")
        .header("Content-Type", "application/json")
        .POST(HttpRequest.BodyPublishers.ofString(json))
        .build()
    val res = http.send(request, HttpResponse.BodyHandlers.ofString())
    if (res.statusCode() >= 400) throw RuntimeException(res.body())
    return res.body()
}

/** Makes an application (a database of its own) with the management token; returns its token. */
fun createApp(name: String, adminToken: String): String {
    val request = HttpRequest.newBuilder(URI.create("$url/api/apps"))
        .header("Authorization", "Bearer $adminToken")
        .header("Content-Type", "application/json")
        .POST(HttpRequest.BodyPublishers.ofString("""{"name": ${quote(name)}}"""))
        .build()
    val res = http.send(request, HttpResponse.BodyHandlers.ofString())
    if (res.statusCode() >= 400) throw RuntimeException(res.body())
    return Regex("\"token\":\"([^\"]+)\"").find(res.body())?.groupValues?.get(1)
        ?: throw RuntimeException("no token in ${res.body()}")
}

fun main() {
    val admin = System.getenv("NUAGE_ADMIN_TOKEN") ?: ""
    if (token.isEmpty() && admin.isNotEmpty()) {
        token = createApp("notes-kotlin", admin)
        println("created the application notes-kotlin")
    }
    sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)")
    println(sql("INSERT INTO notes (text) VALUES (?)", "hello from kotlin"))
    println(sql("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5"))
}
