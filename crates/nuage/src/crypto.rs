//! Encryption at rest: everything Nuage DB writes is encrypted, with the
//! primitives of [brocolis](https://github.com/maxlestage/brocolis) (AES-256,
//! HKDF-SHA256, Argon2id for a passphrase).
//!
//! | What | How |
//! |---|---|
//! | the page store (`nuage.redb`: tables, indexes, schemas, branches, the list of applications) | XTS-AES-256 (IEEE 1619), 4 KiB data units, under a [`StorageBackend`] of redb |
//! | the write-ahead log (`wal/*.log`) | AES-256-GCM per record, under a key of its own for each segment |
//! | backups (snapshots and log segments) | copies of the files above: encrypted the same way |
//!
//! The secret is [`Encryption`]: a 32-byte key (`NUAGE_ENCRYPTION_KEY`, 64 hex
//! digits or base64) or a passphrase. Each data directory has a header,
//! `encryption.json`, with a random salt and a check value (never the key):
//! the directory's root key is HKDF-SHA256(key, salt), or Argon2id(passphrase,
//! salt) for a passphrase (64 MiB, 3 passes, as brocolis' protected keys), and
//! the keys of the pages, of the log and the check value are derived from it
//! with HKDF. Each application has its own directory and salt, hence keys of
//! its own. A wrong key is reported as such at startup, before any data is
//! read.
//!
//! The page store uses XTS, the mode of disk encryption: pages are rewritten
//! in place and keep their size, so there is no room for a nonce or a tag;
//! redb's own checksums catch damaged pages. Log records, written once, get
//! authenticated encryption.

use std::fs::{File, OpenOptions};
use std::io;
use std::ops::Bound;
use std::path::Path;
use std::sync::{Arc, Mutex};

use aes::Aes256;
use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use base64::Engine as _;
use memoire_orm::{Error, Result};
use redb::StorageBackend;
use redb::backends::FileBackend;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::hkdf;
use serde_json::{Value as Json, json};
use zeroize::Zeroizing;

/// Name of the header of an encrypted data directory (and of its copy in the
/// backup).
pub const HEADER_FILE: &str = "encryption.json";
/// What the status and the console call the scheme.
pub const SCHEME: &str = "XTS-AES-256 (pages), AES-256-GCM (log)";

const KEY_LEN: usize = 32;
const SALT_LEN: usize = 32;
const CHECK_LEN: usize = 16;
/// Argon2id for a passphrase: 64 MiB, 3 passes, 1 lane (brocolis' default).
const ARGON2_M_KIB: u32 = 65_536;
const ARGON2_T: u32 = 3;
const ARGON2_P: u32 = 1;
/// The shortest passphrase accepted.
pub const PASSPHRASE_MIN: usize = 12;

fn invalid(message: impl Into<String>) -> Error {
    Error::Validation(message.into())
}

pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("the operating system has no random source");
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// The secret everything is encrypted with: a key or a passphrase.
#[derive(Clone)]
pub struct Encryption {
    secret: Secret,
}

#[derive(Clone)]
enum Secret {
    Key(Zeroizing<[u8; KEY_LEN]>),
    Passphrase(Zeroizing<String>),
}

impl std::fmt::Debug for Encryption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encryption").field("secret", &self.kind()).finish()
    }
}

impl Encryption {
    /// A 32-byte key.
    pub fn from_key(key: [u8; KEY_LEN]) -> Encryption {
        Encryption { secret: Secret::Key(Zeroizing::new(key)) }
    }

    /// A passphrase (at least [`PASSPHRASE_MIN`] characters), stretched with
    /// Argon2id.
    pub fn from_passphrase(passphrase: &str) -> Result<Encryption> {
        if passphrase.chars().count() < PASSPHRASE_MIN {
            return Err(invalid(format!(
                "the encryption passphrase is too short: use at least {PASSPHRASE_MIN} characters, or a key (openssl rand -hex 32)"
            )));
        }
        Ok(Encryption { secret: Secret::Passphrase(Zeroizing::new(passphrase.to_owned())) })
    }

    /// `NUAGE_ENCRYPTION_KEY`: 64 hex digits or the base64 of 32 bytes is a
    /// key; anything else, a passphrase.
    pub fn parse(secret: &str) -> Result<Encryption> {
        let secret = secret.trim();
        if secret.len() == 64
            && let Some(bytes) = unhex(secret)
        {
            return Ok(Encryption::from_key(bytes.try_into().map_err(|_| invalid("bad key"))?));
        }
        let b64 = base64::engine::general_purpose::STANDARD
            .decode(secret)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(secret.trim_end_matches('=')));
        if let Ok(bytes) = b64
            && let Ok(key) = <[u8; KEY_LEN]>::try_from(bytes.as_slice())
        {
            return Ok(Encryption::from_key(key));
        }
        Encryption::from_passphrase(secret)
    }

    /// A new random key, as 64 hex digits (what `NUAGE_ENCRYPTION_KEY` takes).
    pub fn generate_key() -> String {
        hex(&random::<KEY_LEN>())
    }

    /// `key` or `passphrase`.
    pub fn kind(&self) -> &'static str {
        match self.secret {
            Secret::Key(_) => "key",
            Secret::Passphrase(_) => "passphrase",
        }
    }

    fn root(&self, salt: &[u8; SALT_LEN]) -> Result<Zeroizing<[u8; KEY_LEN]>> {
        let mut root = Zeroizing::new([0u8; KEY_LEN]);
        match &self.secret {
            Secret::Key(key) => hkdf_into(salt, key.as_ref(), b"nuage/v1/root", root.as_mut()),
            Secret::Passphrase(p) => {
                let params = argon2::Params::new(ARGON2_M_KIB, ARGON2_T, ARGON2_P, Some(KEY_LEN))
                    .map_err(|e| invalid(format!("argon2: {e}")))?;
                argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
                    .hash_password_into(p.as_bytes(), salt, root.as_mut())
                    .map_err(|e| invalid(format!("argon2: {e}")))?;
            }
        }
        Ok(root)
    }
}

struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn hkdf_into(salt: &[u8], ikm: &[u8], info: &[u8], out: &mut [u8]) {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(ikm);
    let info = [info];
    // Infallible for outputs up to 255 × 32 bytes.
    prk.expand(&info, Len(out.len())).and_then(|okm| okm.fill(out)).expect("HKDF output length");
}

fn derive<const N: usize>(root: &[u8], info: &[u8]) -> Zeroizing<[u8; N]> {
    let mut out = Zeroizing::new([0u8; N]);
    hkdf_into(&[], root, info, out.as_mut());
    out
}

/// The header of an encrypted directory: how its root key is derived, and a
/// value that tells a wrong key from a right one. No secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    kind: &'static str,
    salt: [u8; SALT_LEN],
    check: [u8; CHECK_LEN],
}

/// The keys of one data directory.
#[derive(Clone)]
pub(crate) struct Keys {
    pub pages: Arc<Xts>,
    pub wal: Arc<WalKey>,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keys(..)")
    }
}

impl Header {
    /// A new header (fresh salt) and the keys it opens.
    pub(crate) fn create(encryption: &Encryption) -> Result<(Header, Keys)> {
        let salt = random::<SALT_LEN>();
        let root = encryption.root(&salt)?;
        let check = *derive::<CHECK_LEN>(root.as_ref(), b"nuage/v1/check");
        Ok((Header { kind: encryption.kind(), salt, check }, keys(root.as_ref())))
    }

    /// The keys, or an error if `encryption` is not the secret this
    /// directory was encrypted with.
    pub(crate) fn unlock(&self, encryption: &Encryption) -> Result<Keys> {
        if encryption.kind() != self.kind {
            return Err(invalid(format!(
                "this data was encrypted with a {}, not a {}: NUAGE_ENCRYPTION_KEY is not the right one",
                self.kind,
                encryption.kind()
            )));
        }
        let root = encryption.root(&self.salt)?;
        let check = derive::<CHECK_LEN>(root.as_ref(), b"nuage/v1/check");
        if !crate::server::same_secret(check.as_ref(), &self.check) {
            return Err(invalid("wrong encryption key: this data was encrypted with another NUAGE_ENCRYPTION_KEY"));
        }
        Ok(keys(root.as_ref()))
    }

    pub(crate) fn to_json(&self) -> Vec<u8> {
        let kdf = match self.kind {
            "passphrase" => json!({ "name": "argon2id", "m_kib": ARGON2_M_KIB, "t": ARGON2_T, "p": ARGON2_P }),
            _ => json!({ "name": "hkdf-sha256" }),
        };
        let header = json!({
            "format": "nuage-encryption/1",
            "scheme": SCHEME,
            "secret": self.kind,
            "kdf": kdf,
            "salt": hex(&self.salt),
            "check": hex(&self.check),
        });
        serde_json::to_vec_pretty(&header).unwrap_or_default()
    }

    pub(crate) fn from_json(bytes: &[u8]) -> Result<Header> {
        let bad = || invalid(format!("{HEADER_FILE} is damaged"));
        let j: Json = serde_json::from_slice(bytes).map_err(|_| bad())?;
        if j.get("format").and_then(Json::as_str) != Some("nuage-encryption/1") {
            return Err(invalid(format!("{HEADER_FILE}: unknown format (written by a newer Nuage DB?)")));
        }
        let field = |k: &str| j.get(k).and_then(Json::as_str).and_then(unhex).ok_or_else(bad);
        Ok(Header {
            kind: match j.get("secret").and_then(Json::as_str) {
                Some("passphrase") => "passphrase",
                Some("key") => "key",
                _ => return Err(bad()),
            },
            salt: field("salt")?.try_into().map_err(|_| bad())?,
            check: field("check")?.try_into().map_err(|_| bad())?,
        })
    }

    /// The header of `dir`, if it is encrypted.
    pub(crate) fn read(dir: &Path) -> Result<Option<Header>> {
        match std::fs::read(dir.join(HEADER_FILE)) {
            Ok(bytes) => Header::from_json(&bytes).map(Some),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(crate::engine::storage_err(format!("cannot read {HEADER_FILE}: {e}"))),
        }
    }

    /// Writes it into `dir` (aside, then renamed).
    pub(crate) fn write(&self, dir: &Path) -> Result<()> {
        let io = |e: io::Error| crate::engine::storage_err(format!("cannot write {HEADER_FILE}: {e}"));
        let tmp = dir.join(format!("{HEADER_FILE}.partial"));
        std::fs::write(&tmp, self.to_json()).map_err(io)?;
        File::open(&tmp).and_then(|f| f.sync_all()).map_err(io)?;
        std::fs::rename(&tmp, dir.join(HEADER_FILE)).map_err(io)?;
        crate::engine::wal::sync_dir(dir);
        Ok(())
    }
}

fn keys(root: &[u8]) -> Keys {
    let pages = derive::<64>(root, b"nuage/v1/pages");
    let wal = derive::<KEY_LEN>(root, b"nuage/v1/wal");
    Keys { pages: Arc::new(Xts::new(pages.as_ref())), wal: Arc::new(WalKey(wal)) }
}

// ---------------------------------------------------------------------------
// XTS-AES-256, for the page store
// ---------------------------------------------------------------------------

const BLOCK: usize = 16;
/// The XTS data unit: 4 KiB (the tweak is the unit's number).
const UNIT: u64 = 4096;

/// XTS-AES-256 (IEEE 1619-2007): data unit `n` (`UNIT` bytes at offset
/// `n × UNIT`), block `j` in it: `C = E_K1(P ⊕ T) ⊕ T` with
/// `T = E_K2(n) ⊗ αʲ`.
pub(crate) struct Xts {
    data: Aes256,
    tweak: Aes256,
}

/// `t ⊗ α` in GF(2¹²⁸), little-endian (IEEE 1619).
fn double(t: &mut [u8; BLOCK]) {
    let carry = t[15] >> 7;
    for i in (1..BLOCK).rev() {
        t[i] = (t[i] << 1) | (t[i - 1] >> 7);
    }
    t[0] = (t[0] << 1) ^ (0x87 * carry);
}

impl Xts {
    fn new(key: &[u8]) -> Xts {
        Xts {
            data: Aes256::new_from_slice(&key[..32]).expect("32-byte key"),
            tweak: Aes256::new_from_slice(&key[32..64]).expect("32-byte key"),
        }
    }

    fn tweak_at(&self, offset: u64) -> [u8; BLOCK] {
        let mut unit = [0u8; BLOCK];
        unit[..8].copy_from_slice(&(offset / UNIT).to_le_bytes());
        let mut block = aes::Block::from(unit);
        self.tweak.encrypt_block(&mut block);
        let mut t: [u8; BLOCK] = block.into();
        for _ in 0..(offset % UNIT) / BLOCK as u64 {
            double(&mut t);
        }
        t
    }

    /// Encrypts (`encrypt`) or decrypts `buf`, which sits at `offset` in the
    /// file. Both are multiples of 16. Decrypting, a block of zeros (space
    /// the file grew by, never written) stays zeros.
    fn apply(&self, offset: u64, buf: &mut [u8], encrypt: bool) {
        debug_assert!(offset.is_multiple_of(BLOCK as u64) && buf.len().is_multiple_of(BLOCK));
        let mut at = offset;
        let mut t = self.tweak_at(at);
        for chunk in buf.chunks_exact_mut(BLOCK) {
            if at.is_multiple_of(UNIT) && at != offset {
                t = self.tweak_at(at);
            }
            if encrypt || chunk.iter().any(|b| *b != 0) {
                let mut block = aes::Block::default();
                for i in 0..BLOCK {
                    block[i] = chunk[i] ^ t[i];
                }
                if encrypt {
                    self.data.encrypt_block(&mut block);
                } else {
                    self.data.decrypt_block(&mut block);
                }
                for i in 0..BLOCK {
                    chunk[i] = block[i] ^ t[i];
                }
            }
            double(&mut t);
            at += BLOCK as u64;
        }
    }
}

/// The page store's file, encrypted: what redb reads and writes goes
/// through XTS. Writes that do not cover whole 16-byte blocks read, patch and
/// rewrite the blocks at their edges.
pub(crate) struct EncryptedFile {
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
    pub(crate) fn open(path: &Path, xts: Arc<Xts>) -> io::Result<EncryptedFile> {
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

    fn query_lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> std::result::Result<bool, redb::BackendError> {
        self.inner.query_lock_range(start, end)
    }
}

// ---------------------------------------------------------------------------
// AES-256-GCM, for the write-ahead log
// ---------------------------------------------------------------------------

/// Length of the salt at the head of an encrypted log segment.
pub(crate) const SEGMENT_SALT: usize = 32;
const TAG: usize = 16;

/// The log's key. Each segment draws a random salt (in its header); its key
/// is HKDF(log key, salt), and the nonce of a record its LSN, which is unique
/// in a segment: no nonce is ever used twice under one key, even after a
/// restore replays LSNs.
pub(crate) struct WalKey(Zeroizing<[u8; KEY_LEN]>);

/// The key of one segment.
pub(crate) struct SegmentKey(LessSafeKey);

impl WalKey {
    pub(crate) fn segment(&self, salt: &[u8; SEGMENT_SALT]) -> SegmentKey {
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        hkdf_into(salt, self.0.as_ref(), b"nuage/v1/wal-segment", key.as_mut());
        SegmentKey(LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key.as_ref()).expect("AES-256 key")))
    }
}

fn nonce(lsn: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&lsn.to_le_bytes());
    Nonce::assume_unique_for_key(n)
}

impl SegmentKey {
    /// `lsn (8) ‖ AES-256-GCM(record) ‖ tag`, the LSN authenticated.
    pub(crate) fn seal(&self, lsn: u64, record: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + record.len() + TAG);
        out.extend_from_slice(&lsn.to_le_bytes());
        let mut body = record.to_vec();
        self.0
            .seal_in_place_append_tag(nonce(lsn), Aad::from(lsn.to_le_bytes()), &mut body)
            .expect("AES-GCM seals any record");
        out.extend_from_slice(&body);
        out
    }

    /// The record sealed by [`seal`](Self::seal), `None` if it was altered
    /// or sealed under another key.
    pub(crate) fn open(&self, sealed: &[u8]) -> Option<Vec<u8>> {
        let lsn = u64::from_le_bytes(sealed.get(..8)?.try_into().ok()?);
        let mut body = sealed.get(8..)?.to_vec();
        let plain = self.0.open_in_place(nonce(lsn), Aad::from(lsn.to_le_bytes()), &mut body).ok()?;
        Some(plain.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xts_matches_ieee_1619() {
        // Reference: Python's `cryptography` (OpenSSL), XTS-AES-256 over data
        // unit 7 (tweak 7, little-endian) of 4096 bytes.
        let key = unhex("0b30557a9fc4e90e33587da2c7ec11365b80a5caef14395e83a8cdf2173c6186abd0f51a3f6489aed3f81d42678cb1d6fb20456a8fb4d9fe23486d92b7dc0126").unwrap();
        let xts = Xts::new(&key);
        let plain: Vec<u8> = (0..4096u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut unit = plain.clone();
        xts.apply(7 * 4096, &mut unit, true);
        let digest = ring::digest::digest(&ring::digest::SHA256, &unit);
        assert_eq!(hex(digest.as_ref()), "ec9a52b0ff1e7a3e79ca853436d07c74f0b08be6a9e4b7f6ae23e34dcf077773");
        // A range that starts inside the unit gives the same bytes.
        let mut part = plain[32..96].to_vec();
        xts.apply(7 * 4096 + 32, &mut part, true);
        assert_eq!(
            hex(&part),
            "a4019eec77878ba40f4df9c789e546fee4fcc771257b0279e62d2d3a386c7974322e93dfbdf88d81cc8e79265b21c26770a0ae60088c083d23779dd4be3da3e6"
        );
        xts.apply(7 * 4096, &mut unit, false);
        assert_eq!(unit, plain);
    }

    #[test]
    fn encrypted_file_reads_back_unaligned_writes() {
        let dir = std::env::temp_dir().join(format!("nuage-crypto-{}", std::process::id()));
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

    #[test]
    fn keys_secrets_and_headers() {
        let key = Encryption::parse(&"ab".repeat(32)).unwrap();
        assert_eq!(key.kind(), "key");
        let b64 = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        assert_eq!(Encryption::parse(&b64).unwrap().kind(), "key");
        assert!(Encryption::parse("short").is_err());
        assert_eq!(Encryption::generate_key().len(), 64);
        assert!(!format!("{key:?}").contains("ab"), "no secret in Debug");

        let (header, keys) = Header::create(&key).unwrap();
        let back = Header::from_json(&header.to_json()).unwrap();
        assert_eq!(back, header);
        let again = back.unlock(&key).unwrap();
        let salt = [3u8; SEGMENT_SALT];
        let sealed = keys.wal.segment(&salt).seal(42, b"record");
        assert_eq!(again.wal.segment(&salt).open(&sealed).as_deref(), Some(&b"record"[..]));
        assert!(keys.wal.segment(&[4; SEGMENT_SALT]).open(&sealed).is_none(), "another segment's key");
        let mut tampered = sealed.clone();
        tampered[0] ^= 1;
        assert!(keys.wal.segment(&salt).open(&tampered).is_none(), "the LSN is authenticated");
        let wrong = Encryption::from_key([1; 32]);
        assert!(header.unlock(&wrong).unwrap_err().to_string().contains("wrong encryption key"));
        let phrase = Encryption::from_passphrase("correct horse battery staple").unwrap();
        assert!(header.unlock(&phrase).is_err());
        let (h2, _) = Header::create(&phrase).unwrap();
        assert!(h2.unlock(&phrase).is_ok());
        assert!(String::from_utf8(h2.to_json()).unwrap().contains("argon2id"));
    }
}
