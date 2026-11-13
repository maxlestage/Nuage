//! Nuage DB from Rust over HTTP: `cargo run` in this folder.
use serde_json::{Value, json};

/// Runs SQL with `?` parameters; returns `{"columns", "rows"}` or `{"rows_affected", "last_insert_id"}`.
fn sql(url: &str, token: &str, query: &str, params: Value) -> Result<Value, String> {
    let mut res = ureq::post(format!("{url}/v1/sql"))
        .header("Authorization", format!("Bearer {token}"))
        .config()
        .http_status_as_error(false) // read the error's body too
        .build()
        .send_json(json!({ "sql": query, "params": params }))
        .map_err(|e| e.to_string())?;
    let body: Value = res.body_mut().read_json().map_err(|e| e.to_string())?;
    match body.get("error") {
        Some(e) => Err(format!("{}: {}", e["kind"], e["message"])),
        None => Ok(body),
    }
}

fn main() -> Result<(), String> {
    let url = std::env::var("NUAGE_URL").unwrap_or_else(|_| "http://localhost:8080".into());
    let token = std::env::var("NUAGE_TOKEN").map_err(|_| "set NUAGE_TOKEN")?;
    sql(&url, &token, "CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)", json!([]))?;
    let inserted = sql(&url, &token, "INSERT INTO notes (text) VALUES (?)", json!(["hello from rust"]))?;
    println!("inserted {}", inserted["last_insert_id"]);
    let page = sql(&url, &token, "SELECT id, text FROM notes ORDER BY id DESC LIMIT 5", json!([]))?;
    for row in page["rows"].as_array().into_iter().flatten() {
        println!("{} {}", row["id"], row["text"].as_str().unwrap_or_default());
    }
    Ok(())
}
