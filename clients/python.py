"""Nuage DB from Python 3, with the standard library only.

    NUAGE_URL=https://db.example.com NUAGE_TOKEN=... python3 python.py

Without NUAGE_TOKEN, with the server's NUAGE_ADMIN_TOKEN: it makes its own application first.
"""
import json
import os
import urllib.error
import urllib.request

URL = os.environ.get("NUAGE_URL", "http://localhost:8080")
TOKEN = os.environ.get("NUAGE_TOKEN")


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


def create_app(name, admin_token):
    """Makes an application (a database of its own) with the management token; returns its token."""
    request = urllib.request.Request(
        f"{URL}/api/apps",
        data=json.dumps({"name": name}).encode(),
        headers={"Authorization": f"Bearer {admin_token}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request) as response:
            return json.load(response)["token"]
    except urllib.error.HTTPError as e:
        error = json.load(e)["error"]
        raise RuntimeError(f"{error['kind']}: {error['message']}") from None


if not TOKEN and os.environ.get("NUAGE_ADMIN_TOKEN"):
    TOKEN = create_app("notes-python", os.environ["NUAGE_ADMIN_TOKEN"])
    print("created the application notes-python")

sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)")
print("inserted", sql("INSERT INTO notes (text) VALUES (?)", ["hello from python"])["last_insert_id"])
for row in sql("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5")["rows"]:
    print(row["id"], row["text"])
