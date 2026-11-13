// Nuage DB from Swift (iOS, macOS, Linux), with Foundation only.
//   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... swift swift.swift
import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

let base = ProcessInfo.processInfo.environment["NUAGE_URL"] ?? "http://localhost:8080"
let token = ProcessInfo.processInfo.environment["NUAGE_TOKEN"] ?? ""

struct NuageError: Error { let message: String }

/// Runs SQL with ? parameters; returns ["columns", "rows"] or ["rows_affected", "last_insert_id"].
func sql(_ query: String, _ params: [Any] = []) throws -> [String: Any] {
    var request = URLRequest(url: URL(string: base + "/v1/sql")!)
    request.httpMethod = "POST"
    request.setValue("Bearer " + token, forHTTPHeaderField: "Authorization")
    request.setValue("application/json", forHTTPHeaderField: "Content-Type")
    request.httpBody = try JSONSerialization.data(withJSONObject: ["sql": query, "params": params])
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

_ = try sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)")
print("inserted", try sql("INSERT INTO notes (text) VALUES (?)", ["hello from swift"])["last_insert_id"] ?? "")
for row in try sql("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5")["rows"] as? [[String: Any]] ?? [] {
    print(row["id"] ?? "", row["text"] ?? "")
}
