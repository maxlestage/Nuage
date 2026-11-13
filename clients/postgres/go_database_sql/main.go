// Nuage DB with Go's database/sql and lib/pq (or pgx/stdlib); GORM, sqlx and ent use them.
// The password is a token of an application.
//
//	NUAGE_TOKEN=... go run .
package main

import (
	"database/sql"
	"fmt"
	"os"

	_ "github.com/lib/pq"
)

func env(name, fallback string) string {
	if v := os.Getenv(name); v != "" {
		return v
	}
	return fallback
}

func main() {
	dsn := fmt.Sprintf("host=%s port=%s user=app dbname=nuage password=%s sslmode=disable",
		env("NUAGE_PG_HOST", "localhost"), env("NUAGE_PG_PORT", "5432"), os.Getenv("NUAGE_TOKEN"))
	db, err := sql.Open("postgres", dsn)
	if err != nil {
		panic(err)
	}
	defer db.Close()
	if _, err := db.Exec("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)"); err != nil {
		panic(err)
	}
	var id int64
	if err := db.QueryRow("INSERT INTO notes (text) VALUES ($1) RETURNING id", "hello from database/sql").Scan(&id); err != nil {
		panic(err)
	}
	fmt.Println("inserted", id)
	rows, err := db.Query("SELECT id, text FROM notes WHERE id <= $1 ORDER BY id DESC LIMIT 5", id)
	if err != nil {
		panic(err)
	}
	defer rows.Close()
	for rows.Next() {
		var n int64
		var text string
		if err := rows.Scan(&n, &text); err != nil {
			panic(err)
		}
		fmt.Println(n, text)
	}
}
