// Nuage DB with pgx (go get github.com/jackc/pgx/v5), Go's PostgreSQL driver; database/sql
// with lib/pq or pgx/stdlib works too. The password is a token of an application.
//
//	NUAGE_TOKEN=... go run .
package main

import (
	"context"
	"fmt"
	"os"

	"github.com/jackc/pgx/v5"
)

func env(name, fallback string) string {
	if v := os.Getenv(name); v != "" {
		return v
	}
	return fallback
}

func main() {
	ctx := context.Background()
	url := fmt.Sprintf("postgres://app:%s@%s:%s/nuage", os.Getenv("NUAGE_TOKEN"), env("NUAGE_PG_HOST", "localhost"), env("NUAGE_PG_PORT", "5432"))
	conn, err := pgx.Connect(ctx, url)
	if err != nil {
		panic(err)
	}
	defer conn.Close(ctx)
	if _, err := conn.Exec(ctx, "CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)"); err != nil {
		panic(err)
	}
	tx, err := conn.Begin(ctx)
	if err != nil {
		panic(err)
	}
	var id int64
	if err := tx.QueryRow(ctx, "INSERT INTO notes (text) VALUES ($1) RETURNING id", "hello from pgx").Scan(&id); err != nil {
		panic(err)
	}
	if err := tx.Commit(ctx); err != nil {
		panic(err)
	}
	fmt.Println("inserted", id)
	rows, err := conn.Query(ctx, "SELECT id, text FROM notes WHERE id <= $1 ORDER BY id DESC LIMIT 5", id)
	if err != nil {
		panic(err)
	}
	for rows.Next() {
		var n int64
		var text string
		if err := rows.Scan(&n, &text); err != nil {
			panic(err)
		}
		fmt.Println(n, text)
	}
	if err := rows.Err(); err != nil {
		panic(err)
	}
}
