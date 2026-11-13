// Nuage DB from Kotlin (JVM, Android with any HTTP client), with the JDK only (add
// kotlinx.serialization, Moshi or Gson to read the rows as objects).
//   kotlinc kotlin.kt -include-runtime -d nuage.jar && NUAGE_TOKEN=... java -jar nuage.jar
import java.net.URI
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse

val url = System.getenv("NUAGE_URL") ?: "http://localhost:8080"
val token: String = System.getenv("NUAGE_TOKEN")
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

fun main() {
    sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)")
    println(sql("INSERT INTO notes (text) VALUES (?)", "hello from kotlin"))
    println(sql("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5"))
}
