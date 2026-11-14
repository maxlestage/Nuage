// Nuage DB from Swift (iOS, macOS, Linux), with Foundation only.
//   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... swift swift.swift
//   Without NUAGE_TOKEN, with the server's NUAGE_ADMIN_TOKEN: it makes its own application first.
import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

let base = ProcessInfo.processInfo.environment["NUAGE_URL"] ?? "http://localhost:8080"
var token = ProcessInfo.processInfo.environment["NUAGE_TOKEN"] ?? ""

struct NuageError: Error { let message: String }

/// POSTs JSON to `path` with a bearer token; returns the JSON answer, or throws its error.
func post(_ path: String, _ json: [String: Any], bearer: String) throws -> [String: Any] {
    var request = URLRequest(url: URL(string: base + path)!)
    request.httpMethod = "POST"
    request.setValue("Bearer " + bearer, forHTTPHeaderField: "Authorization")
    request.setValue("application/json", forHTTPHeaderField: "Content-Type")
    request.httpBody = try JSONSerialization.data(withJSONObject: json)
    var answer: Data?
    var failure: Error?
    let done = DispatchSemaphore(value: 0)
    URLSession.shared.dataTask(with: request) { data, _, error in
        answer = data
        failure = error
        done.signal()
    }.resume()
    done.wait()
    if let failure { throw failure }
    let body = try JSONSerialization.jsonObject(with: answer ?? Data()) as? [String: Any] ?? [:]
    if let error = body["error"] as? [String: Any] {
        throw NuageError(message: "\(error["kind"] ?? ""): \(error["message"] ?? "")")
    }
    return body
}

/// Runs SQL with ? parameters; returns ["columns", "rows"] or ["rows_affected", "last_insert_id"].
func sql(_ query: String, _ params: [Any] = []) throws -> [String: Any] {
    try post("/v1/sql", ["sql": query, "params": params], bearer: token)
}

/// Makes an application (a database of its own) with the management token; returns its token.
func createApp(_ name: String, adminToken: String) throws -> String {
    guard let made = try post("/api/apps", ["name": name], bearer: adminToken)["token"] as? String else {
        throw NuageError(message: "no token in the answer")
    }
    return made
}

if token.isEmpty, let admin = ProcessInfo.processInfo.environment["NUAGE_ADMIN_TOKEN"], !admin.isEmpty {
    token = try createApp("notes-swift", adminToken: admin)
    print("created the application notes-swift")
}

_ = try sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)")
print("inserted", try sql("INSERT INTO notes (text) VALUES (?)", ["hello from swift"])["last_insert_id"] ?? "")
for row in try sql("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5")["rows"] as? [[String: Any]] ?? [] {
    print(row["id"] ?? "", row["text"] ?? "")
}
