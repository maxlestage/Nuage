<?php
// Nuage DB with PDO (the pdo_pgsql extension); Laravel and Symfony (Doctrine) use it.
// The password is a token of an application.
//   NUAGE_TOKEN=... php php_pdo.php
$host = getenv('NUAGE_PG_HOST') ?: 'localhost';
$port = getenv('NUAGE_PG_PORT') ?: '5432';
$db = new PDO("pgsql:host=$host;port=$port;dbname=nuage", 'app', getenv('NUAGE_TOKEN'), [
    PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION,
]);
$db->exec('CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)');
$db->beginTransaction();
$insert = $db->prepare('INSERT INTO notes (text) VALUES (?) RETURNING id');
$insert->execute(['hello from pdo']);
$id = $insert->fetchColumn();
$db->commit();
echo "inserted $id\n";
$select = $db->prepare('SELECT id, text FROM notes WHERE id <= ? ORDER BY id DESC LIMIT 5');
$select->execute([$id]);
foreach ($select as $row) {
    echo $row['id'], ' ', $row['text'], "\n";
}
