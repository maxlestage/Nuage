// Nuage DB from Go, with the standard library only.
//
//	NUAGE_URL=https://db.example.com NUAGE_TOKEN=... go run go.go
//
// Without NUAGE_TOKEN, with the server's NUAGE_ADMIN_TOKEN: it makes its own application first.
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
)

// Result is the answer to one statement.
type Result struct {
	Columns      []string         `json:"columns"`
	Rows         []map[string]any `json:"rows"`
	RowsAffected int64            `json:"rows_affected"`
	LastInsertID any              `json:"last_insert_id"`
	Error        *struct {
		Kind    string `json:"kind"`
		Message string `json:"message"`
	} `json:"error"`
}

var url, token = env("NUAGE_URL", "http://localhost:8080"), os.Getenv("NUAGE_TOKEN")

func env(name, fallback string) string {
	if v := os.Getenv(name); v != "" {
		return v
	}
	return fallback
}

// SQL runs a query with ? parameters.
func SQL(query string, params ...any) (*Result, error) {
	if params == nil {
		params = []any{}
	}
	body, _ := json.Marshal(map[string]any{"sql": query, "params": params})
	req, _ := http.NewRequest("POST", url+"/v1/sql", bytes.NewReader(body))
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("Content-Type", "application/json")
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer res.Body.Close()
	var r Result
	if err := json.NewDecoder(res.Body).Decode(&r); err != nil {
		return nil, err
	}
	if r.Error != nil {
		return nil, fmt.Errorf("%s: %s", r.Error.Kind, r.Error.Message)
	}
	return &r, nil
}

// CreateApp makes an application (a database of its own) with the management token and
// returns its token.
func CreateApp(name, adminToken string) (string, error) {
	body, _ := json.Marshal(map[string]any{"name": name})
	req, _ := http.NewRequest("POST", url+"/api/apps", bytes.NewReader(body))
	req.Header.Set("Authorization", "Bearer "+adminToken)
	req.Header.Set("Content-Type", "application/json")
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		return "", err
	}
	defer res.Body.Close()
	var r struct {
		Token string `json:"token"`
		Error *struct {
			Kind    string `json:"kind"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if err := json.NewDecoder(res.Body).Decode(&r); err != nil {
		return "", err
	}
	if r.Error != nil {
		return "", fmt.Errorf("%s: %s", r.Error.Kind, r.Error.Message)
	}
	return r.Token, nil
}

func main() {
	if admin := os.Getenv("NUAGE_ADMIN_TOKEN"); token == "" && admin != "" {
		t, err := CreateApp("notes-go", admin)
		if err != nil {
			panic(err)
		}
		token = t
		fmt.Println("created the application notes-go")
	}
	must := func(r *Result, err error) *Result {
		if err != nil {
			panic(err)
		}
		return r
	}
	must(SQL("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)"))
	fmt.Println("inserted", must(SQL("INSERT INTO notes (text) VALUES (?)", "hello from go")).LastInsertID)
	for _, row := range must(SQL("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5")).Rows {
		fmt.Println(row["id"], row["text"])
	}
}
