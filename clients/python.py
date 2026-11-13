"""Nuage DB from Python 3, with the standard library only.

    NUAGE_URL=https://db.example.com NUAGE_TOKEN=... python3 python.py
"""
import json
import os
import urllib.error
import urllib.request

URL = os.environ.get("NUAGE_URL", "http://localhost:8080")
TOKEN = os.environ["NUAGE_TOKEN"]


def sql(query, params=()):
    """Runs SQL with ? parameters; returns {"columns", "rows"} or {"rows_affected", "last_insert_id"}."""
    request = urllib.request.Request(
        f"{URL}/v1/sql",
        data=json.dumps({"sql": query, "params": list(params)}).encode(),
        headers={"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request) as response:
            return json.load(response)
    except urllib.error.HTTPError as e:
        error = json.load(e)["error"]
        raise RuntimeError(f"{error['kind']}: {error['message']}") from None


sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)")
print("inserted", sql("INSERT INTO notes (text) VALUES (?)", ["hello from python"])["last_insert_id"])
for row in sql("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5")["rows"]:
    print(row["id"], row["text"])
