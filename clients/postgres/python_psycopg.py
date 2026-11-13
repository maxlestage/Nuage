"""Nuage DB with psycopg 3 (pip install "psycopg[binary]"); psycopg2, SQLAlchemy and Django
connect the same way. The password is a token of an application.

    NUAGE_TOKEN=... python3 python_psycopg.py
"""
import os

import psycopg

conninfo = (
    f"host={os.environ.get('NUAGE_PG_HOST', 'localhost')} port={os.environ.get('NUAGE_PG_PORT', '5432')} "
    f"user=app dbname=nuage password={os.environ['NUAGE_TOKEN']}"
)
with psycopg.connect(conninfo) as conn:  # one transaction, committed at the end of the block
    conn.execute("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)")
    row = conn.execute("INSERT INTO notes (text) VALUES (%s) RETURNING id", ["hello from psycopg"]).fetchone()
    print("inserted", row[0])
with psycopg.connect(conninfo) as conn:
    for id, text in conn.execute("SELECT id, text FROM notes WHERE id <= %s ORDER BY id DESC LIMIT 5", [row[0]]):
        print(id, text)
