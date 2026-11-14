//! Encryption at rest: everything Nuage DB writes is encrypted (AES-256,
//! HKDF-SHA256, Argon2id for a passphrase).
//!
//! | What | How |
//! |---|---|
//! | the page store (`nuage.db`: tables, indexes, schemas, branches, the list of applications) | XTS-AES-256 (IEEE 1619), one 4 KiB data unit per page |
//! | the write-ahead log (`wal/*.log`) | AES-256-GCM per record, under a key of its own for each segment |
//! | backups (snapshots and log segments) | copies of the files above: encrypted the same way |
//!
//! The secret is [`Encryption`]: a 32-byte key (`NUAGE_ENCRYPTION_KEY`, 64 hex
//! digits or base64) or a passphrase. Each data directory has a header,
//! `encryption.json`, with a random salt and a check value (never the key):
//! the directory's root key is HKDF-SHA256(key, salt), or Argon2id(passphrase,
//! salt) for a passphrase (64 MiB, 3 passes), and
//! the keys of the pages, of the log and the check value are derived from it
//! with HKDF. Each application has its own directory and salt, hence keys of
//! its own. A wrong key is reported as such at startup, before any data is
//! read.
//!
//! The page store uses XTS, the mode of disk encryption: a page keeps its
//! place and its size, so there is no room for a nonce or a tag; the checksum
//! each page carries, inside the encryption, catches damaged pages. Log
//! records, written once, get authenticated encryption.

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;

use aes::Aes256;
use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use aes_gcm::aead::AeadInOut;
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use hkdf::Hkdf;
use memoire_orm::{Error, Result};
use serde_json::{Value as Json, json};
use sha2::Sha256;
use zeroize::Zeroizing;

/// Name of the header of an encrypted data directory (and of its copy in the
/// backup).
pub const HEADER_FILE: &str = "encryption.json";
/// What the status and the console call the scheme.
pub const SCHEME: &str = "XTS-AES-256 (pages), AES-256-GCM (log)";

const KEY_LEN: usize = 32;
const SALT_LEN: usize = 32;
const CHECK_LEN: usize = 16;
/// Argon2id for a passphrase: 64 MiB, 3 passes, 1 lane (OWASP's recommendation is lower: this is safer).
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

/// HKDF-SHA256 (RFC 5869). An empty salt is HMAC's empty key, the same as a salt of zeros.
fn hkdf_into(salt: &[u8], ikm: &[u8], info: &[u8], out: &mut [u8]) {
    // Infallible for outputs up to 255 × 32 bytes.
    Hkdf::<Sha256>::new(Some(salt), ikm).expand(info, out).expect("HKDF output length");
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
        if !crate::hash::same_secret(check.as_ref(), &self.check) {
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

    /// Encrypts `buf`, which sits at `offset` in the file. Both are multiples
    /// of 16.
    pub(crate) fn encrypt(&self, offset: u64, buf: &mut [u8]) {
        self.run(offset, buf, true);
    }

    /// Decrypts `buf`, which sits at `offset` in the file.
    pub(crate) fn decrypt(&self, offset: u64, buf: &mut [u8]) {
        self.run(offset, buf, false);
    }

    /// The blocks of each data unit in one call to AES, which runs several
    /// blocks at once where the processor can.
    fn run(&self, offset: u64, buf: &mut [u8], encrypt: bool) {
        debug_assert!(offset.is_multiple_of(BLOCK as u64) && buf.len().is_multiple_of(BLOCK));
        let mut tweaks = [[0u8; BLOCK]; (UNIT as usize) / BLOCK];
        let mut at = offset;
        let mut rest = buf;
        while !rest.is_empty() {
            let n = rest.len().min((UNIT - at % UNIT) as usize);
            let (part, tail) = rest.split_at_mut(n);
            let blocks = part.as_chunks_mut::<BLOCK>().0;
            let mut t = self.tweak_at(at);
            for (block, tweak) in blocks.iter_mut().zip(tweaks.iter_mut()) {
                *tweak = t;
                for i in 0..BLOCK {
                    block[i] ^= t[i];
                }
                double(&mut t);
            }
            let cast = aes::Block::cast_slice_from_core_mut(blocks);
            if encrypt {
                self.data.encrypt_blocks(cast);
            } else {
                self.data.decrypt_blocks(cast);
            }
            for (block, tweak) in blocks.iter_mut().zip(tweaks.iter()) {
                for i in 0..BLOCK {
                    block[i] ^= tweak[i];
                }
            }
            at += n as u64;
            rest = tail;
        }
    }

    /// As the page store of versions up to 0.4 (redb) used it: encrypts
    /// (`encrypt`) or decrypts `buf` at `offset`; decrypting, a block of zeros
    /// (space the file grew by, never written) stays zeros.
    #[cfg(any(feature = "import-redb", test))]
    pub(crate) fn apply(&self, offset: u64, buf: &mut [u8], encrypt: bool) {
        debug_assert!(offset.is_multiple_of(BLOCK as u64) && buf.len().is_multiple_of(BLOCK));
        let mut at = offset;
        let mut t = self.tweak_at(at);
        for chunk in buf.as_chunks_mut::<BLOCK>().0 {
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
pub(crate) struct SegmentKey(Aes256Gcm);

impl WalKey {
    pub(crate) fn segment(&self, salt: &[u8; SEGMENT_SALT]) -> SegmentKey {
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        hkdf_into(salt, self.0.as_ref(), b"nuage/v1/wal-segment", key.as_mut());
        SegmentKey(Aes256Gcm::new_from_slice(key.as_ref()).expect("AES-256 key"))
    }
}

/// The nonce of a record: its LSN, little-endian, in the last 8 of 12 bytes (unique per
/// segment key, as each segment has a key of its own).
fn nonce(lsn: u64) -> Nonce<aes_gcm::aead::consts::U12> {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&lsn.to_le_bytes());
    n.into()
}

impl SegmentKey {
    /// [`seal`](Self::seal) where the record is: `buf` holds the LSN then the
    /// record from `at` on; the record is encrypted in place and the tag appended.
    pub(crate) fn seal_in_place(&self, lsn: u64, buf: &mut Vec<u8>, at: usize) {
        let tag = self
            .0
            .encrypt_inout_detached(&nonce(lsn), &lsn.to_le_bytes(), (&mut buf[at..]).into())
            .expect("AES-GCM seals any record");
        buf.extend_from_slice(&tag);
    }

    /// `lsn (8) ‖ AES-256-GCM(record) ‖ tag`, the LSN authenticated.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn seal(&self, lsn: u64, record: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + record.len() + TAG);
        out.extend_from_slice(&lsn.to_le_bytes());
        let mut body = record.to_vec();
        self.0.encrypt_in_place(&nonce(lsn), &lsn.to_le_bytes(), &mut body).expect("AES-GCM seals any record");
        out.extend_from_slice(&body);
        out
    }

    /// The record sealed by [`seal`](Self::seal), `None` if it was altered
    /// or sealed under another key.
    pub(crate) fn open(&self, sealed: &[u8]) -> Option<Vec<u8>> {
        let lsn = u64::from_le_bytes(sealed.get(..8)?.try_into().ok()?);
        let mut body = sealed.get(8..)?.to_vec();
        self.0.decrypt_in_place(&nonce(lsn), &lsn.to_le_bytes(), &mut body).ok()?;
        Some(body)
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
        let digest = crate::hash::sha256(&unit);
        assert_eq!(hex(&digest), "ec9a52b0ff1e7a3e79ca853436d07c74f0b08be6a9e4b7f6ae23e34dcf077773");
        // A range that starts inside the unit gives the same bytes.
        let mut part = plain[32..96].to_vec();
        xts.apply(7 * 4096 + 32, &mut part, true);
        assert_eq!(
            hex(&part),
            "a4019eec77878ba40f4df9c789e546fee4fcc771257b0279e62d2d3a386c7974322e93dfbdf88d81cc8e79265b21c26770a0ae60088c083d23779dd4be3da3e6"
        );
        xts.apply(7 * 4096, &mut unit, false);
        assert_eq!(unit, plain);
        // The page store's path (whole blocks at once) gives the same bytes, across units.
        let long: Vec<u8> = (0..3 * 4096u32).map(|i| (i * 13 % 241) as u8).collect();
        let (mut one, mut many) = (long.clone(), long.clone());
        xts.apply(6 * 4096 + 64, &mut one, true);
        xts.encrypt(6 * 4096 + 64, &mut many);
        assert_eq!(one, many);
        xts.decrypt(6 * 4096 + 64, &mut many);
        assert_eq!(many, long);
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
#[cfg(test)]
mod compatibility {
    //! Outputs of the `ring` implementation (before RustCrypto), so that data and backups
    //! written by earlier versions keep opening.
    use super::*;

    #[test]
    fn hkdf_and_aes_gcm_match_the_ring_implementation() {
        let mut out = [0u8; 42];
        hkdf_into(b"sel", b"cle d'entree", b"info", &mut out);
        assert_eq!(hex(&out), "2d9e31783f6bf605bbf22cd1b968151e227d082827415367962351ad7e45ed0a039793de1f91e99971be");
        let mut out = [0u8; 32];
        hkdf_into(&[], b"racine", b"nuage/v1/pages", &mut out);
        assert_eq!(hex(&out), "7bc6adab37db370ad9539c6d6076808aab2e8432da64e271ea832c74674f2d2f");
        let segment = WalKey(Zeroizing::new([7u8; KEY_LEN])).segment(&[9u8; SEGMENT_SALT]);
        let sealed = segment.seal(42, b"enregistrement du journal");
        assert_eq!(
            hex(&sealed),
            "2a00000000000000b9677a048c6284c885ae23cb3de5be6dea77411668b12ea89c73a6fa2c20fa9e6ab63df2bdf64dd5ea"
        );
        assert_eq!(segment.open(&sealed).as_deref(), Some(&b"enregistrement du journal"[..]));
        // Sealed where it is (what the log does): the same bytes.
        let mut in_place = 42u64.to_le_bytes().to_vec();
        in_place.extend_from_slice(b"enregistrement du journal");
        segment.seal_in_place(42, &mut in_place, 8);
        assert_eq!(in_place, sealed);
        let mut altered = sealed.clone();
        altered[12] ^= 1;
        assert!(segment.open(&altered).is_none());
    }
}
