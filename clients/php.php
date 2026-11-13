<?php
// Nuage DB from PHP 7.4+, without extensions.
//   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... php php.php
$url = getenv('NUAGE_URL') ?: 'http://localhost:8080';
$token = getenv('NUAGE_TOKEN');

/** Runs SQL with ? parameters; returns ['columns', 'rows'] or ['rows_affected', 'last_insert_id']. */
function sql(string $query, array $params = []): array
{
    global $url, $token;
    $context = stream_context_create(['http' => [
        'method' => 'POST',
        'header' => "Authorization: Bearer $token\r\nContent-Type: application/json\r\n",
        'content' => json_encode(['sql' => $query, 'params' => $params]),
        'ignore_errors' => true, // read the error's body too
    ]]);
    $body = json_decode(file_get_contents("$url/v1/sql", false, $context), true);
    if (isset($body['error'])) {
        throw new RuntimeException($body['error']['kind'] . ': ' . $body['error']['message']);
    }
    return $body;
}

sql('CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)');
echo 'inserted ', sql('INSERT INTO notes (text) VALUES (?)', ['hello from php'])['last_insert_id'], "\n";
foreach (sql('SELECT id, text FROM notes ORDER BY id DESC LIMIT 5')['rows'] as $row) {
    echo $row['id'], ' ', $row['text'], "\n";
}
