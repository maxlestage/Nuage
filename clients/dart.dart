// Nuage DB from Dart (and Flutter, with the http package or dart:io), with the SDK only.
//   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... dart run dart.dart
//   Without NUAGE_TOKEN, with the server's NUAGE_ADMIN_TOKEN: it makes its own application first.
import 'dart:convert';
import 'dart:io';

final url = Platform.environment['NUAGE_URL'] ?? 'http://localhost:8080';
var token = Platform.environment['NUAGE_TOKEN'] ?? '';
final http = HttpClient();

/// Runs SQL with ? parameters; returns {columns, rows} or {rows_affected, last_insert_id}.
Future<Map<String, dynamic>> sql(String query, [List<Object?> params = const []]) async {
  final request = await http.postUrl(Uri.parse('$url/v1/sql'));
  request.headers.set('Authorization', 'Bearer $token');
  request.headers.contentType = ContentType.json;
  request.write(jsonEncode({'sql': query, 'params': params}));
  final response = await request.close();
  final body = jsonDecode(await response.transform(utf8.decoder).join()) as Map<String, dynamic>;
  if (body['error'] != null) throw Exception('${body['error']['kind']}: ${body['error']['message']}');
  return body;
}

/// Makes an application (a database of its own) with the management token; returns its token.
Future<String> createApp(String name, String adminToken) async {
  final request = await http.postUrl(Uri.parse('$url/api/apps'));
  request.headers.set('Authorization', 'Bearer $adminToken');
  request.headers.contentType = ContentType.json;
  request.write(jsonEncode({'name': name}));
  final response = await request.close();
  final body = jsonDecode(await response.transform(utf8.decoder).join()) as Map<String, dynamic>;
  if (body['error'] != null) throw Exception('${body['error']['kind']}: ${body['error']['message']}');
  return body['token'] as String;
}

Future<void> main() async {
  final admin = Platform.environment['NUAGE_ADMIN_TOKEN'] ?? '';
  if (token.isEmpty && admin.isNotEmpty) {
    token = await createApp('notes-dart', admin);
    print('created the application notes-dart');
  }
  await sql('CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)');
  print('inserted ${(await sql('INSERT INTO notes (text) VALUES (?)', ['hello from dart']))['last_insert_id']}');
  for (final row in (await sql('SELECT id, text FROM notes ORDER BY id DESC LIMIT 5'))['rows']) {
    print('${row['id']} ${row['text']}');
  }
  http.close();
}
