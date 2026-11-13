//! Nuage DB with tokio-postgres: `cargo run` in this folder.
use tokio_postgres::NoTls;

#[tokio::main]
async fn main() -> Result<(), tokio_postgres::Error> {
    let host = std::env::var("NUAGE_PG_HOST").unwrap_or_else(|_| "localhost".into());
    let port = std::env::var("NUAGE_PG_PORT").unwrap_or_else(|_| "5432".into());
    let token = std::env::var("NUAGE_TOKEN").expect("set NUAGE_TOKEN");
    let (mut client, connection) =
        tokio_postgres::connect(&format!("host={host} port={port} user=app dbname=nuage password={token}"), NoTls)
            .await?;
    tokio::spawn(connection);
    client.batch_execute("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)").await?;
    let tx = client.transaction().await?;
    let id: i64 =
        tx.query_one("INSERT INTO notes (text) VALUES ($1) RETURNING id", &[&"hello from tokio-postgres"]).await?.get(0);
    tx.commit().await?;
    println!("inserted {id}");
    for row in client.query("SELECT id, text FROM notes WHERE id <= $1 ORDER BY id DESC LIMIT 5", &[&id]).await? {
        let (id, text): (i64, String) = (row.get(0), row.get(1));
        println!("{id} {text}");
    }
    Ok(())
}
