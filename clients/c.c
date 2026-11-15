/* Nuage DB from C (or C++), with libcurl. Read the JSON answer with any JSON library
 * (cJSON, jansson, nlohmann/json in C++).
 *   cc c.c -lcurl -o nuage && NUAGE_URL=https://db.example.com NUAGE_TOKEN=... ./nuage
 * Without NUAGE_TOKEN, with the server's NUAGE_ADMIN_TOKEN: it makes its own application first.
 * Build the request's JSON with the same library: parameters are values, never SQL. */
#include <curl/curl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static const char *url(void) {
    const char *u = getenv("NUAGE_URL");
    return u && *u ? u : "http://localhost:8080";
}

/* POSTs a JSON body to /v1/sql and prints the JSON answer; returns 0 when it succeeded. */
static int sql(const char *json) {
    char address[512], auth[512];
    snprintf(address, sizeof address, "%s/v1/sql", url());
    snprintf(auth, sizeof auth, "Authorization: Bearer %s", getenv("NUAGE_TOKEN"));
    CURL *curl = curl_easy_init();
    struct curl_slist *headers = curl_slist_append(NULL, auth);
    headers = curl_slist_append(headers, "Content-Type: application/json");
    curl_easy_setopt(curl, CURLOPT_URL, address);
    curl_easy_setopt(curl, CURLOPT_HTTPHEADER, headers);
    curl_easy_setopt(curl, CURLOPT_POSTFIELDS, json);
    CURLcode rc = curl_easy_perform(curl); /* the answer goes to stdout */
    long status = 0;
    curl_easy_getinfo(curl, CURLINFO_RESPONSE_CODE, &status);
    putchar('\n');
    curl_slist_free_all(headers);
    curl_easy_cleanup(curl);
    return rc == CURLE_OK && status < 400 ? 0 : 1;
}

/* Appends what curl receives to a buffer. */
struct buffer { char data[8192]; size_t len; };
static size_t keep(char *chunk, size_t size, size_t n, void *out) {
    struct buffer *b = out;
    size_t len = size * n, room = sizeof b->data - 1 - b->len;
    memcpy(b->data + b->len, chunk, len < room ? len : room);
    b->len += len < room ? len : room;
    b->data[b->len] = 0;
    return len;
}

/* Makes an application (a database of its own) with the management token, and puts its token
 * in NUAGE_TOKEN; returns 0 when it succeeded. */
static int create_app(const char *name) {
    char address[512], auth[512], json[256];
    struct buffer answer = {.len = 0};
    snprintf(address, sizeof address, "%s/api/apps", url());
    snprintf(auth, sizeof auth, "Authorization: Bearer %s", getenv("NUAGE_ADMIN_TOKEN"));
    snprintf(json, sizeof json, "{\"name\": \"%s\"}", name);
    CURL *curl = curl_easy_init();
    struct curl_slist *headers = curl_slist_append(NULL, auth);
    headers = curl_slist_append(headers, "Content-Type: application/json");
    curl_easy_setopt(curl, CURLOPT_URL, address);
    curl_easy_setopt(curl, CURLOPT_HTTPHEADER, headers);
    curl_easy_setopt(curl, CURLOPT_POSTFIELDS, json);
    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, keep);
    curl_easy_setopt(curl, CURLOPT_WRITEDATA, &answer);
    CURLcode rc = curl_easy_perform(curl);
    curl_slist_free_all(headers);
    curl_easy_cleanup(curl);
    /* "token":"nuage_..." in the answer (a JSON library reads it in one line). */
    char *start = rc == CURLE_OK ? strstr(answer.data, "\"token\":\"") : NULL;
    if (!start) {
        fprintf(stderr, "%s\n", answer.data);
        return 1;
    }
    start += strlen("\"token\":\"");
    char *end = strchr(start, '"');
    if (!end) return 1;
    *end = 0;
    setenv("NUAGE_TOKEN", start, 1);
    printf("created the application %s\n", name);
    return 0;
}

int main(void) {
    curl_global_init(CURL_GLOBAL_DEFAULT);
    const char *token = getenv("NUAGE_TOKEN"), *admin = getenv("NUAGE_ADMIN_TOKEN");
    if ((!token || !*token) && admin && *admin && create_app("notes-c")) return 1;
    int failed = sql("{\"sql\": \"CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)\"}")
        || sql("{\"sql\": \"INSERT INTO notes (text) VALUES (?)\", \"params\": [\"hello from c\"]}")
        || sql("{\"sql\": \"SELECT id, text FROM notes ORDER BY id DESC LIMIT 5\"}");
    curl_global_cleanup();
    return failed;
}
