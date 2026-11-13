/* Nuage DB from C (or C++), with libcurl. Read the JSON answer with any JSON library
 * (cJSON, jansson, nlohmann/json in C++).
 *   cc c.c -lcurl -o nuage && NUAGE_URL=https://db.example.com NUAGE_TOKEN=... ./nuage
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

int main(void) {
    curl_global_init(CURL_GLOBAL_DEFAULT);
    int failed = sql("{\"sql\": \"CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)\"}")
        || sql("{\"sql\": \"INSERT INTO notes (text) VALUES (?)\", \"params\": [\"hello from c\"]}")
        || sql("{\"sql\": \"SELECT id, text FROM notes ORDER BY id DESC LIMIT 5\"}");
    curl_global_cleanup();
    return failed;
}
