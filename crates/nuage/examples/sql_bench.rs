//! Mesure du SQL embarqué (`Embedded::sql`) sur une table de `n` lignes (5 000 par défaut) :
//! écritures en transaction, lectures par clé, filtres indexés ou non, tri paginé, sous-requête.
//!
//! `cargo run --release -p nuage --no-default-features --example sql_bench -- 5000`

use std::time::{Duration, Instant};

use nuage::{Embedded, EngineOptions};
use serde_json::{Value as Json, json};

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(5_000);
    let filter = std::env::args().nth(2);
    let dir = std::env::temp_dir().join(format!("nuage-sql-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Embedded::open_with(&dir, EngineOptions { sync: false, ..EngineOptions::default() }).unwrap();
    db.sql(
        "CREATE TABLE main (id TEXT PRIMARY KEY, espace INTEGER NOT NULL, joue_le BIGINT, resume TEXT NOT NULL);
         CREATE INDEX main_espace ON main (espace);
         CREATE INDEX main_joue_le ON main (joue_le);
         CREATE TABLE joueur (cle TEXT PRIMARY KEY, main TEXT NOT NULL, nom TEXT NOT NULL);
         CREATE INDEX joueur_nom ON joueur (nom)",
        &[],
    )
    .unwrap();
    let resume = "x".repeat(600);
    let t = Instant::now();
    let tx = db.begin_now().unwrap();
    for i in 0..n {
        let id = format!("m{i:06}");
        tx.sql(
            "INSERT INTO main (id, espace, joue_le, resume) VALUES (?, ?, ?, ?); \
             INSERT INTO joueur (cle, main, nom) VALUES (?, ?, ?), (?, ?, ?)",
            &[
                json!(id),
                json!(i % 3),
                json!(i * 1000),
                json!(resume),
                json!(format!("{id}/hero")),
                json!(id),
                json!("Hero"),
                json!(format!("{id}/v")),
                json!(id),
                json!(format!("V{}", i % 50)),
            ],
        )
        .unwrap();
    }
    tx.commit_now().unwrap();
    println!("{:44} {:>10.2?} par ligne", "insertion (une transaction)", t.elapsed() / n as u32);

    let run = |name: &str, reps: u32, sql: &str, params: &[Json]| {
        if filter.as_deref().is_some_and(|f| !name.contains(f)) {
            return;
        }
        let t = Instant::now();
        let mut rows = 0;
        for _ in 0..reps {
            rows = db.sql(sql, params).unwrap().last().map_or(0, |r| r.rows.len());
        }
        let per: Duration = t.elapsed() / reps;
        println!("{name:44} {per:>10.2?} ({rows} lignes)");
    };
    run("clé primaire", 2000, "SELECT resume FROM main WHERE id = ?", &[json!("m000042")]);
    run("count(*)", 200, "SELECT count(*) FROM main", &[]);
    run("count(*) index (1/3)", 50, "SELECT count(*) FROM main WHERE espace = ?", &[json!(1)]);
    run("index (1/3), id+date", 50, "SELECT id, joue_le FROM main WHERE espace = ?", &[json!(1)]);
    run("parcours, id+date", 50, "SELECT id, joue_le FROM main", &[]);
    run("parcours, filtre non indexé", 50, "SELECT id FROM main WHERE resume = 'non'", &[]);
    run("tri paginé (LIMIT 20)", 50, "SELECT id FROM main ORDER BY joue_le DESC NULLS LAST, id LIMIT 20", &[]);
    run(
        "IN (sous-requête, 1/50)",
        50,
        "SELECT id, joue_le FROM main WHERE id IN (SELECT main FROM joueur WHERE nom = ?)",
        &[json!("V7")],
    );
    run(
        "IN (sous-requête, toutes)",
        20,
        "SELECT id, joue_le FROM main WHERE id IN (SELECT main FROM joueur WHERE nom = ?)",
        &[json!("Hero")],
    );
    run(
        "page par IN (20 clés)",
        500,
        &format!("SELECT id, resume FROM main WHERE id IN ({})", vec!["?"; 20].join(", ")),
        &(100..120).map(|i| json!(format!("m{i:06}"))).collect::<Vec<_>>(),
    );
    // Écritures, une validation par instruction (journal écrit, sans `fsync`).
    let mut next = n;
    let mut write = |name: &str, reps: u32, sql: &str, params: &mut dyn FnMut(usize) -> Vec<Json>| {
        if filter.as_deref().is_some_and(|f| !name.contains(f)) {
            return;
        }
        let t = Instant::now();
        for _ in 0..reps {
            db.sql(sql, &params(next)).unwrap();
            next += 1;
        }
        let per: Duration = t.elapsed() / reps;
        println!("{name:44} {per:>10.2?}");
    };
    write(
        "insertion (une validation par ligne)",
        2000,
        "INSERT INTO main (id, espace, joue_le, resume) VALUES (?, ?, ?, ?)",
        &mut |i| vec![json!(format!("m{i:06}")), json!(i % 3), json!(i * 1000), json!(resume)],
    );
    write("mise à jour par clé", 2000, "UPDATE main SET joue_le = ? WHERE id = ?", &mut |i| {
        vec![json!(i), json!(format!("m{:06}", i * 7 % n))]
    });
    write("suppression par clé", 2000, "DELETE FROM joueur WHERE cle = ?", &mut |i| {
        vec![json!(format!("m{:06}/v", i * 3 % n))]
    });
    let t = Instant::now();
    db.engine().checkpoint().unwrap();
    println!("{:44} {:>10.2?}", "point de contrôle", t.elapsed());
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
