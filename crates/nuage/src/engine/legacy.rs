//! Data directories written by Nuage DB up to 0.4.
//!
//! Their page store was a [redb](https://docs.rs/redb) file, `nuage.redb`;
//! the log and the backups are the same as today. Opening such a directory
//! (or restoring such a backup) copies every table of the old file into a
//! page file of this version, once: the old file is kept beside it as
//! `nuage.redb.imported`, to delete when the data has been checked.
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

/// Imports the page store of an earlier version found in `dir`, if any.
/// Returns `true` when it did.
#[cfg(not(feature = "import-redb"))]
pub(crate) fn import(
    dir: &std::path::Path,
    _keys: Option<&crate::crypto::Keys>,
    _cache_bytes: usize,
) -> memoire_orm::Result<bool> {
    if dir.join(LEGACY_FILE).exists() && !dir.join(FILE).exists() {
        return Err(storage_err(format!(
            "{} was written by Nuage DB 0.4 or earlier ({LEGACY_FILE}): open it once with a build that has the \
             `import-redb` Cargo feature of `nuage` (the default features have it) to bring it to this version",
            dir.display()
        )));
    }
    Ok(false)
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

    use super::{FILE, IMPORTED, LEGACY_FILE, storage_err};
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

    pub(crate) fn import(dir: &Path, keys: Option<&Keys>, cache_bytes: usize) -> Result<bool> {
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
            return Ok(false);
        }
        let new = dir.join(FILE);
        if !new.exists() {
            let tmp = dir.join(IMPORTING);
            let _ = std::fs::remove_file(&tmp);
            {
                let db = open(&old, cache_bytes, keys)?;
                let pages = Pages::open(&tmp, cache_bytes, keys.map(|k| k.pages.clone()), true)?;
                copy(&db, &pages)?;
                pages.checkpoint()?;
            }
            std::fs::rename(&tmp, &new).map_err(bad)?;
            wal::sync_dir(dir);
        }
        std::fs::rename(&old, dir.join(IMPORTED)).map_err(bad)?;
        wal::sync_dir(dir);
        Ok(true)
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
    }
}
