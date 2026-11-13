"""Nuage DB with SQLAlchemy 2 (pip install sqlalchemy "psycopg[binary]"), the ORM of Python;
the password is a token of an application.

    NUAGE_TOKEN=... python3 python_sqlalchemy.py
"""
import os

from sqlalchemy import String, create_engine, select
from sqlalchemy.orm import DeclarativeBase, Mapped, Session, mapped_column


class Base(DeclarativeBase):
    pass


class Note(Base):
    __tablename__ = "notes"
    id: Mapped[int] = mapped_column(primary_key=True)
    text: Mapped[str] = mapped_column(String)


host, port = os.environ.get("NUAGE_PG_HOST", "localhost"), os.environ.get("NUAGE_PG_PORT", "5432")
engine = create_engine(f"postgresql+psycopg://app:{os.environ['NUAGE_TOKEN']}@{host}:{port}/nuage")
with engine.begin() as conn:
    conn.exec_driver_sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)")
with Session(engine) as session:
    note = Note(text="hello from sqlalchemy")
    session.add(note)
    session.commit()
    print("inserted", note.id)
    for n in session.scalars(select(Note).where(Note.id <= note.id).order_by(Note.id.desc()).limit(5)):
        print(n.id, n.text)
