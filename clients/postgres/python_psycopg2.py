"""Nuage DB with psycopg2 (pip install psycopg2-binary). The password is a token.

    NUAGE_TOKEN=... python3 python_psycopg2.py
"""
import os

import psycopg2

conn = psycopg2.connect(
    host=os.environ.get("NUAGE_PG_HOST", "localhost"),
    port=os.environ.get("NUAGE_PG_PORT", "5432"),
    user="app",
    dbname="nuage",
    password=os.environ["NUAGE_TOKEN"],
)
with conn, conn.cursor() as cur:  # commits at the end of the block
    cur.execute("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)")
    cur.execute("INSERT INTO notes (text) VALUES (%s) RETURNING id", ("hello from psycopg2",))
    print("inserted", cur.fetchone()[0])
    cur.execute("SELECT id, text FROM notes ORDER BY id DESC LIMIT %s", (5,))
    for id, text in cur.fetchall():
        print(id, text)
conn.close()
