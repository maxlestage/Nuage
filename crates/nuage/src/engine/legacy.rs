//! Data directories written by Nuage DB up to 0.4.
//!
//! Their page store was a [redb](https://docs.rs/redb) file, `nuage.redb`;
//! the log and the backups are the same as today. Opening such a directory
//! (or restoring such a backup) copies every table of the old file into a
//! page file of this version, once. The new file is then read back from the
//! disk and compared with the old one, entry by entry, before it takes its
//! place: a difference stops the opening and leaves the old file as it was.
//! The old file is kept beside the new one as `nuage.redb.imported`, to
//! delete when the data has been checked.
//!
//! Reading the old file takes the `import-redb` feature (on by default, off
//! for an application that embeds the engine: `default-features = false`);
//! without it, such a directory is refused with a message that says so.
//! Nothing else in Nuage DB uses redb.

use super::storage_err;
use super::store::FILE;

/// The page store of versions up to 0.4.
pub(crate) const LEGACY_FILE: &str = "nuage.redb";
/// What it is renamed to once imported.
pub(crate) const IMPORTED: &str = "nuage.redb.imported";

/// What an import brought to this version: every entry of it was read back
/// from the new file and compared with the old one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Imported {
    /// Entries: rows, index entries, the catalog's, of every branch.
    pub entries: u64,
    /// Bytes of their keys and values.
    pub bytes: u64,
}

/// Imports the page store of an earlier version found in `dir`, if any.
/// Returns what it imported when it did.
#[cfg(not(feature = "import-redb"))]
pub(crate) fn import(
    dir: &std::path::Path,
    _keys: Option<&crate::crypto::Keys>,
    _cache_bytes: usize,
) -> memoire_orm::Result<Option<Imported>> {
    if dir.join(LEGACY_FILE).exists() && !dir.join(FILE).exists() {
        return Err(storage_err(format!(
            "{} was written by Nuage DB 0.4 or earlier ({LEGACY_FILE}): open it once with a build that has the \
             `import-redb` Cargo feature of `nuage` (the default features have it) to bring it to this version",
            dir.display()
        )));
    }
    Ok(None)
}

#[cfg(feature = "import-redb")]
pub(crate) use redb_file::import;

#[cfg(feature = "import-redb")]
mod redb_file {
    use std::fs::OpenOptions;
    use std::io;
    use std::ops::Bound;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use memoire_orm::Result;
    use redb::backends::FileBackend;
    use redb::{ReadableDatabase, ReadableTable, StorageBackend, TableDefinition, TableHandle};

    use super::{FILE, IMPORTED, Imported, LEGACY_FILE, storage_err};
    use crate::crypto::{Keys, Xts};
    use crate::engine::pages::Pages;
    use crate::engine::wal::{self, Op};

    /// Where the new page file is written before it takes its place.
    const IMPORTING: &str = "nuage.db.importing";
    /// An encrypted copy the old version made of a store in clear, not yet put in place.
    const OLD_ENCRYPTING: &str = "nuage.redb.encrypting";
    /// Bytes of entries applied at once, and kept in memory before they are written.
    const BATCH: usize = 4 << 20;

    const BLOCK: usize = 16;

    fn bad(e: impl std::fmt::Display) -> memoire_orm::Error {
        storage_err(format!("importing {LEGACY_FILE}: {e}"))
    }

    pub(crate) fn import(dir: &Path, keys: Option<&Keys>, cache_bytes: usize) -> Result<Option<Imported>> {
        let old = dir.join(LEGACY_FILE);
        let encrypting = dir.join(OLD_ENCRYPTING);
        if encrypting.exists() {
            if keys.is_some() {
                // The old version had encrypted the store and written the
                // header: its copy is the store, the log in clear is covered.
                std::fs::rename(&encrypting, &old).map_err(bad)?;
                for (_, path) in wal::segments(&dir.join("wal"))? {
                    std::fs::remove_file(&path).map_err(bad)?;
                }
            } else {
                // No header yet: the store in clear is whole, the copy is not.
                std::fs::remove_file(&encrypting).map_err(bad)?;
            }
        }
        if !old.exists() {
            return Ok(None);
        }
        let new = dir.join(FILE);
        let xts = || keys.map(|k| k.pages.clone());
        let imported = {
            let db = open(&old, cache_bytes, keys)?;
            if new.exists() {
                // An import that stopped between its two renames: the new file
                // is in place, and nothing was written to it since.
                same(&db, &Pages::open(&new, cache_bytes, xts(), true)?).map_err(|e| {
                    bad(format!(
                        "{} holds both {FILE} and {LEGACY_FILE}, and they differ ({e}): move away the one that is \
                         not the data",
                        dir.display()
                    ))
                })?
            } else {
                let tmp = dir.join(IMPORTING);
                let _ = std::fs::remove_file(&tmp);
                {
                    let pages = Pages::open(&tmp, cache_bytes, xts(), true)?;
                    copy(&db, &pages)?;
                    pages.checkpoint()?;
                }
                // Read back from the disk before it is trusted: the old file
                // stays the data until every entry was found in the new one.
                let checked = Pages::open(&tmp, cache_bytes, xts(), true).and_then(|pages| {
                    pages.check().map_err(bad)?;
                    same(&db, &pages).map_err(|e| bad(format!("the copy differs from {LEGACY_FILE}: {e}")))
                });
                let imported = match checked {
                    Ok(imported) => imported,
                    Err(e) => {
                        let _ = std::fs::remove_file(&tmp);
                        return Err(e);
                    }
                };
                std::fs::rename(&tmp, &new).map_err(bad)?;
                wal::sync_dir(dir);
                imported
            }
        };
        std::fs::rename(&old, dir.join(IMPORTED)).map_err(bad)?;
        wal::sync_dir(dir);
        Ok(Some(imported))
    }

    fn open(path: &Path, cache_bytes: usize, keys: Option<&Keys>) -> Result<redb::Database> {
        let mut builder = redb::Builder::new();
        builder.set_cache_size(cache_bytes);
        match keys {
            Some(k) => {
                let file = EncryptedFile::open(path, k.pages.clone())
                    .map_err(|e| bad(format!("cannot open {}: {e}", path.display())))?;
                builder.create_with_backend(file).map_err(|e| {
                    bad(format!("{} ({e}): is it damaged, or encrypted with another key?", path.display()))
                })
            }
            None => builder.open(path).map_err(bad),
        }
    }

    /// Every entry of every table, in key order, into the page file.
    fn copy(from: &redb::Database, to: &Pages) -> Result<()> {
        let r = from.begin_read().map_err(bad)?;
        for handle in r.list_tables().map_err(bad)? {
            let name = handle.name().to_owned();
            let table = r.open_table(TableDefinition::<&[u8], &[u8]>::new(&name)).map_err(bad)?;
            let mut ops = Vec::new();
            let mut bytes = 0;
            for item in table.iter().map_err(bad)? {
                let (k, v) = item.map_err(bad)?;
                bytes += k.value().len() + v.value().len();
                ops.push(Op::Put { table: name.clone(), key: k.value().to_vec(), value: v.value().to_vec() });
                if bytes >= BATCH {
                    to.apply(&ops, 0)?;
                    ops.clear();
                    bytes = 0;
                    if to.dirty_bytes() >= 4 * BATCH {
                        to.checkpoint()?;
                    }
                }
            }
            to.apply(&ops, 0)?;
        }
        Ok(())
    }

    /// Compares the page file with the old store, entry by entry, both in key
    /// order: the same tables, the same keys, the same values, nothing more.
    /// Says what differs when something does.
    fn same(from: &redb::Database, to: &Pages) -> std::result::Result<Imported, String> {
        let version = to.version();
        let r = from.begin_read().map_err(|e| e.to_string())?;
        let mut imported = Imported::default();
        let mut names = std::collections::BTreeSet::new();
        for handle in r.list_tables().map_err(|e| e.to_string())? {
            let name = handle.name().to_owned();
            let table = r.open_table(TableDefinition::<&[u8], &[u8]>::new(&name)).map_err(|e| e.to_string())?;
            let mut old = table.iter().map_err(|e| e.to_string())?;
            let (mut entries, mut bytes) = (0u64, 0u64);
            let mut wrong = None;
            if let Some(root) = version.root(&name) {
                to.walk_all(&root, &mut |k, v| {
                    wrong = match old.next() {
                        Some(Ok((ok, ov))) if ok.value() == k && ov.value() == v => None,
                        Some(Ok((ok, _))) if ok.value() == k => {
                            Some(format!("entry {} has another value", entries + 1))
                        }
                        Some(Ok(_)) => Some(format!("entry {} has another key", entries + 1)),
                        Some(Err(e)) => Some(e.to_string()),
                        None => Some(format!("it has more than the {entries} entries of the old one")),
                    };
                    entries += 1;
                    bytes += (k.len() + v.len()) as u64;
                    wrong.is_none()
                })
                .map_err(|e| format!("table `{name}`: {e}"))?;
            }
            if wrong.is_none() {
                wrong = match old.next() {
                    Some(Ok(_)) => Some(format!("it has only {entries} of the entries of the old one")),
                    Some(Err(e)) => Some(e.to_string()),
                    None => None,
                };
            }
            if let Some(wrong) = wrong {
                return Err(format!("table `{name}`: {wrong}"));
            }
            if bytes != version.bytes(&name) {
                return Err(format!("table `{name}`: holds {bytes} bytes, {} recorded", version.bytes(&name)));
            }
            imported.entries += entries;
            imported.bytes += bytes;
            names.insert(name);
        }
        match version.names().into_iter().find(|n| !names.contains(n)) {
            Some(extra) => Err(format!("table `{extra}` is not in the old one")),
            None => Ok(imported),
        }
    }

    /// The old page store's file, encrypted: what redb reads and writes goes
    /// through XTS. Writes that do not cover whole 16-byte blocks read, patch
    /// and rewrite the blocks at their edges.
    struct EncryptedFile {
        inner: FileBackend,
        xts: Arc<Xts>,
        writes: Mutex<()>,
    }

    impl std::fmt::Debug for EncryptedFile {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("EncryptedFile")
        }
    }

    impl EncryptedFile {
        fn open(path: &Path, xts: Arc<Xts>) -> io::Result<EncryptedFile> {
            let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
            let inner = FileBackend::new(file).map_err(io::Error::other)?;
            Ok(EncryptedFile { inner, xts, writes: Mutex::new(()) })
        }

        /// The plaintext of the block at `at` (zeros past the end of the file).
        fn block(&self, at: u64, len: u64) -> io::Result<[u8; BLOCK]> {
            let mut block = [0u8; BLOCK];
            if at < len {
                let n = (len - at).min(BLOCK as u64) as usize;
                self.inner.read(at, &mut block[..n])?;
                self.xts.apply(at, &mut block, false);
            }
            Ok(block)
        }
    }

    impl StorageBackend for EncryptedFile {
        fn len(&self) -> io::Result<u64> {
            self.inner.len()
        }

        fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
            let start = offset - offset % BLOCK as u64;
            let end = (offset + out.len() as u64).div_ceil(BLOCK as u64) * BLOCK as u64;
            let mut buf = vec![0u8; (end - start) as usize];
            let len = self.inner.len()?;
            if offset + out.len() as u64 > len {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past the end of the page store"));
            }
            let have = (len.min(end) - start) as usize;
            self.inner.read(start, &mut buf[..have])?;
            self.xts.apply(start, &mut buf, false);
            let skip = (offset - start) as usize;
            out.copy_from_slice(&buf[skip..skip + out.len()]);
            Ok(())
        }

        fn set_len(&self, len: u64) -> io::Result<()> {
            let _w = self.writes.lock().unwrap_or_else(|e| e.into_inner());
            self.inner.set_len(len)
        }

        fn sync_data(&self) -> io::Result<()> {
            self.inner.sync_data()
        }

        fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
            let _w = self.writes.lock().unwrap_or_else(|e| e.into_inner());
            let b = BLOCK as u64;
            let start = offset - offset % b;
            let end = (offset + data.len() as u64).div_ceil(b) * b;
            let mut buf = vec![0u8; (end - start) as usize];
            let skip = (offset - start) as usize;
            if skip != 0 || !data.len().is_multiple_of(BLOCK) {
                let len = self.inner.len()?;
                let first = self.block(start, len)?;
                buf[..BLOCK].copy_from_slice(&first);
                if end - b != start {
                    let last = self.block(end - b, len)?;
                    let n = buf.len();
                    buf[n - BLOCK..].copy_from_slice(&last);
                }
            }
            buf[skip..skip + data.len()].copy_from_slice(data);
            self.xts.apply(start, &mut buf, true);
            self.inner.write(start, &buf)
        }

        fn close(&self) -> io::Result<()> {
            self.inner.close()
        }

        fn try_lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> std::result::Result<bool, redb::BackendError> {
            self.inner.try_lock_range(start, end)
        }

        fn try_lock_shared_range(
            &self,
            start: Bound<u64>,
            end: Bound<u64>,
        ) -> std::result::Result<bool, redb::BackendError> {
            self.inner.try_lock_shared_range(start, end)
        }

        fn lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> std::result::Result<(), redb::BackendError> {
            self.inner.lock_range(start, end)
        }

        fn lock_shared_range(&self, start: Bound<u64>, end: Bound<u64>) -> std::result::Result<(), redb::BackendError> {
            self.inner.lock_shared_range(start, end)
        }

        fn unlock_range(&self, start: Bound<u64>, end: Bound<u64>) -> std::result::Result<(), redb::BackendError> {
            self.inner.unlock_range(start, end)
        }

        fn query_lock_range(
            &self,
            start: Bound<u64>,
            end: Bound<u64>,
        ) -> std::result::Result<bool, redb::BackendError> {
            self.inner.query_lock_range(start, end)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::crypto::{Encryption, Header};

        #[test]
        fn encrypted_file_reads_back_unaligned_writes() {
            let dir = std::env::temp_dir().join(format!("nuage-legacy-crypto-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let (_, keys) = Header::create(&Encryption::from_key([7; 32])).unwrap();
            let path = dir.join("f");
            let f = EncryptedFile::open(&path, keys.pages.clone()).unwrap();
            f.set_len(3 * 4096).unwrap();
            let mut zeros = vec![1u8; 100];
            f.read(4000, &mut zeros).unwrap();
            assert!(zeros.iter().all(|b| *b == 0), "grown space reads as zeros");
            f.write(4090, b"hello, nuage").unwrap();
            f.write(5, b"x").unwrap();
            let mut out = [0u8; 12];
            f.read(4090, &mut out).unwrap();
            assert_eq!(&out, b"hello, nuage");
            let mut one = [0u8; 3];
            f.read(4, &mut one).unwrap();
            assert_eq!(one, [0, b'x', 0]);
            drop(f);
            let raw = std::fs::read(&path).unwrap();
            assert!(!raw.windows(5).any(|w| w == b"hello"), "nothing in clear on disk");
            let _ = std::fs::remove_dir_all(&dir);
        }

        fn temp(name: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!("nuage-legacy-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn put(table: &str, key: &[u8], value: &[u8]) -> Op {
            Op::Put { table: table.to_owned(), key: key.to_vec(), value: value.to_vec() }
        }

        /// The copy is compared with the old store: a missing entry, one too
        /// many, another key, another value or another table is noticed.
        #[test]
        fn a_copy_that_differs_is_noticed() {
            let dir = temp("same");
            let db = redb::Database::create(dir.join("old.redb")).unwrap();
            let w = db.begin_write().unwrap();
            let mut ops = Vec::new();
            for name in ["meta", "b:main"] {
                let mut t = w.open_table(TableDefinition::<&[u8], &[u8]>::new(name)).unwrap();
                for i in 0..2000u32 {
                    let (k, v) = (i.to_be_bytes(), vec![i as u8; (i % 300) as usize]);
                    t.insert(&k[..], &v[..]).unwrap();
                    ops.push(put(name, &k, &v));
                }
            }
            // A table with nothing in it: no tree stands for it.
            w.open_table(TableDefinition::<&[u8], &[u8]>::new("empty")).unwrap();
            w.commit().unwrap();
            let n = std::cell::Cell::new(0);
            let copy = |extra: &[Op]| {
                n.set(n.get() + 1);
                let pages = Pages::open(&dir.join(format!("{}.db", n.get())), 1 << 20, None, false).unwrap();
                pages.apply(&ops, 0).unwrap();
                pages.apply(extra, 0).unwrap();
                pages.checkpoint().unwrap();
                same(&db, &pages)
            };
            let whole = copy(&[]).unwrap();
            assert_eq!(whole.entries, 4000);
            assert_eq!(whole.bytes, 2 * (0..2000u64).map(|i| 4 + i % 300).sum::<u64>());
            let key = |i: u32| i.to_be_bytes();
            let missing = copy(&[Op::Del { table: "b:main".into(), key: key(1999).to_vec() }]).unwrap_err();
            assert!(missing.contains("`b:main`") && missing.contains("only 1999"), "{missing}");
            let hole = copy(&[Op::Del { table: "meta".into(), key: key(7).to_vec() }]).unwrap_err();
            assert!(hole.contains("`meta`") && hole.contains("entry 8 has another key"), "{hole}");
            let more = copy(&[put("meta", &key(5000), b"x")]).unwrap_err();
            assert!(more.contains("more than the 2000 entries"), "{more}");
            let value = copy(&[put("b:main", &key(40), b"other")]).unwrap_err();
            assert!(value.contains("entry 41 has another value"), "{value}");
            let table = copy(&[put("b:other", b"k", b"v")]).unwrap_err();
            assert!(table.contains("`b:other` is not in the old one"), "{table}");
            let filled = copy(&[put("empty", b"k", b"v")]).unwrap_err();
            assert!(filled.contains("`empty`") && filled.contains("more than the 0 entries"), "{filled}");
            drop(db);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// An import that stopped once the new file was in place, before the
        /// old one was set aside, goes on; two files that differ are refused.
        #[test]
        fn an_import_that_stopped_between_its_two_renames() {
            let dir = temp("resumed");
            let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/redb-0.4/clear-closed");
            std::fs::copy(fixture.join(LEGACY_FILE), dir.join(LEGACY_FILE)).unwrap();
            let first = import(&dir, None, 1 << 20).unwrap().expect("imported");
            assert!(first.entries > 352 && first.bytes > 20_000, "{first:?}");
            assert!(dir.join(FILE).exists() && dir.join(IMPORTED).exists() && !dir.join(LEGACY_FILE).exists());
            assert!(!dir.join(IMPORTING).exists());
            assert_eq!(import(&dir, None, 1 << 20).unwrap(), None, "nothing left to import");

            std::fs::rename(dir.join(IMPORTED), dir.join(LEGACY_FILE)).unwrap();
            assert_eq!(import(&dir, None, 1 << 20).unwrap(), Some(first), "the same data, compared again");
            assert!(dir.join(IMPORTED).exists() && !dir.join(LEGACY_FILE).exists());

            // Another file in the place of the new one: refused, both stay.
            {
                let pages = Pages::open(&dir.join(FILE), 1 << 20, None, false).unwrap();
                pages.apply(&[put("meta", b"added", b"later")], 0).unwrap();
                pages.checkpoint().unwrap();
            }
            std::fs::rename(dir.join(IMPORTED), dir.join(LEGACY_FILE)).unwrap();
            let other = std::fs::read(dir.join(FILE)).unwrap();
            let e = import(&dir, None, 1 << 20).unwrap_err().to_string();
            assert!(e.contains("they differ") && e.contains("`meta`"), "{e}");
            assert!(dir.join(LEGACY_FILE).exists() && !dir.join(IMPORTED).exists());
            assert_eq!(std::fs::read(dir.join(FILE)).unwrap(), other);
            // The old file is still the data it was.
            std::fs::remove_file(dir.join(FILE)).unwrap();
            assert_eq!(import(&dir, None, 1 << 20).unwrap(), Some(first));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
