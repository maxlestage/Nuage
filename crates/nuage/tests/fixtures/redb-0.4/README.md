Data directories and backups written by Nuage DB 0.4.0 (commit `997927b`), whose page store
was a redb file (`nuage.redb`). `tests/legacy.rs` opens copies of them with this version.

Each holds a table `notes` (400 rows, one of them 20 kB, 50 deleted, one updated), a dropped
table and a branch `preview` with changes of its own.

| Directory | What it is |
|---|---|
| `clear-closed` | a data directory closed normally |
| `clear-crashed`, `encrypted-crashed` | a data directory copied while open: its last commits are only in the log |
| `clear-backup`, `encrypted-backup` | a backup: a snapshot and a log segment written after it |

The encrypted ones use the key `2a` × 32 (`Encryption::from_key([42; 32])`).
