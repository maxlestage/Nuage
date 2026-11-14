//! The backend-agnostic test suite of Memoire, on an embedded Nuage DB.
#[path = "common/mod.rs"]
mod common;

use nuage::{Embedded, EngineOptions};

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nuage-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn embedded_suite() {
    let dir = temp_dir("suite");
    let db = Embedded::open_with(&dir, EngineOptions { sync: false, ..EngineOptions::default() }).unwrap();
    common::run_suite(&db).await.unwrap();
    common::advanced::run(&db).await.unwrap();
    common::lifecycle::run(&db).await.unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Un modèle qui écrit ses décimaux en texte exact (un `NUMERIC` sous
/// PostgreSQL) dans une table dont la colonne est un `DOUBLE PRECISION` :
/// la valeur est rangée en nombre, et les comparaisons sur la colonne
/// marchent encore (avant, une seule ligne en texte les faisait toutes
/// échouer : « cannot compare text with f64 »).
#[derive(memoire_orm::prelude::Model, Debug, Clone, PartialEq)]
#[memoire(table = "places")]
struct Place {
    #[memoire(primary_key)]
    id: i64,
    latitude: Option<String>,
    note: String,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn texte_ecrit_dans_une_colonne_de_nombres() {
    use memoire_orm::prelude::*;
    let dir = temp_dir("coerce");
    let db = Embedded::open_with(&dir, EngineOptions { sync: false, ..EngineOptions::default() }).unwrap();
    db.sql("CREATE TABLE places (id BIGINT PRIMARY KEY, latitude DOUBLE PRECISION, note TEXT NOT NULL)", &[]).unwrap();
    db.sql("INSERT INTO places VALUES (1, 44.8, 'copiée')", &[]).unwrap();
    let mut p = Place { id: 2, latitude: Some("48.86".into()), note: "12".into() };
    p.insert(&db).await.unwrap();
    let rows = |sql: &str| {
        let r = db.sql(sql, &[]).unwrap().pop().unwrap();
        r.rows.iter().map(|row| row.iter().map(nuage::sql::value_to_json).collect::<Vec<_>>()).collect::<Vec<_>>()
    };
    assert_eq!(
        rows("SELECT typeof(latitude), typeof(note) FROM places WHERE id = 2"),
        vec![vec![serde_json::json!("f64"), serde_json::json!("text")]]
    );
    assert_eq!(rows("SELECT id FROM places WHERE latitude BETWEEN 48 AND 49"), vec![vec![serde_json::json!(2)]]);
    // Un texte qui n'est pas un nombre reste tel quel. (Relue, la valeur
    // convertie est un nombre : le modèle qui la lit doit l'accepter, comme
    // un décimal qui se lit d'un texte ou d'un nombre.)
    Place { id: 3, latitude: Some("n/a".into()), note: String::new() }.insert(&db).await.unwrap();
    assert_eq!(rows("SELECT typeof(latitude) FROM places WHERE id = 3"), vec![vec![serde_json::json!("text")]]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
