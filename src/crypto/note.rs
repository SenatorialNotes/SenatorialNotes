//! Versioned encrypted-note containers.
//!
//! The cleartext header contains only format/KDF data and a stable UUID. Title,
//! body, tags, and private metadata are serialized inside the authenticated
//! ciphertext. Header bytes are supplied as AEAD associated data so changing
//! parameters, the UUID, salt, nonce, or length is detected during unlock.

use std::path::PathBuf;

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::model::{Note, NoteMetadata};
use crate::{Error, Result};

const MAGIC: &[u8; 8] = b"SNOTE\0\0\0";
const FORMAT_VERSION: u16 = 1;
pub(crate) const SALT_LEN: usize = 16;
pub(crate) const NONCE_LEN: usize = 24;
pub(crate) const KEY_LEN: usize = 32;
const HEADER_LEN: usize = 88;

/// Production Argon2id parameters: 64 MiB memory, three passes, one lane.
pub const ARGON2_MEMORY_KIB: u32 = 65_536;
pub const ARGON2_ITERATIONS: u32 = 3;
pub const ARGON2_LANES: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncryptedHeader {
    pub note_id: Uuid,
    pub memory_kib: u32,
    pub iterations: u32,
    pub lanes: u32,
    pub salt: [u8; SALT_LEN],
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext_len: u64,
}

/// Key material retained only for the lifetime of an unlocked note.
/// `Zeroizing` clears it when the session is dropped or explicitly locked.
pub struct EncryptedSession {
    key: Zeroizing<[u8; KEY_LEN]>,
    salt: [u8; SALT_LEN],
    memory_kib: u32,
    iterations: u32,
    lanes: u32,
}

impl std::fmt::Debug for EncryptedSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EncryptedSession")
            .field("key", &"[REDACTED]")
            .field("memory_kib", &self.memory_kib)
            .field("iterations", &self.iterations)
            .field("lanes", &self.lanes)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
struct SensitivePayload {
    metadata: NoteMetadata,
    body: String,
}

impl SensitivePayload {
    fn clear(&mut self) {
        self.metadata.clear_sensitive();
        zeroize::Zeroize::zeroize(&mut self.body);
    }
}

impl Drop for SensitivePayload {
    fn drop(&mut self) {
        self.clear();
    }
}

pub fn encrypt_new(note: &Note, password: &str) -> Result<(Vec<u8>, EncryptedSession)> {
    let mut salt = [0_u8; SALT_LEN];
    getrandom::fill(&mut salt).map_err(|error| Error::Encryption(error.to_string()))?;
    let key = derive_key(
        password,
        &salt,
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_LANES,
    )?;
    let session = EncryptedSession {
        key,
        salt,
        memory_kib: ARGON2_MEMORY_KIB,
        iterations: ARGON2_ITERATIONS,
        lanes: ARGON2_LANES,
    };
    let bytes = encrypt_with_session(note, &session)?;
    Ok((bytes, session))
}

pub fn encrypt_with_session(note: &Note, session: &EncryptedSession) -> Result<Vec<u8>> {
    let mut nonce = [0_u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|error| Error::Encryption(error.to_string()))?;

    let payload = SensitivePayload {
        metadata: note.metadata.clone(),
        body: note.body.clone(),
    };
    let plaintext = Zeroizing::new(
        serde_json::to_vec(&payload).map_err(|error| Error::Encryption(error.to_string()))?,
    );
    let ciphertext_len = plaintext
        .len()
        .checked_add(16)
        .ok_or_else(|| Error::Encryption("note is too large".into()))?
        as u64;
    let header = EncryptedHeader {
        note_id: note.metadata.id,
        memory_kib: session.memory_kib,
        iterations: session.iterations,
        lanes: session.lanes,
        salt: session.salt,
        nonce,
        ciphertext_len,
    };
    let aad = encode_header(&header);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(session.key.as_ref()));
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext.as_ref(),
                aad: &aad,
            },
        )
        .map_err(|_| Error::Encryption("authenticated encryption failed".into()))?;

    let mut container = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    container.extend_from_slice(&aad);
    container.extend_from_slice(&ciphertext);
    Ok(container)
}

pub fn decrypt(
    container: &[u8],
    password: &str,
    relative_path: PathBuf,
) -> Result<(Note, EncryptedSession)> {
    let header = inspect_header(container)?;
    let key = derive_key(
        password,
        &header.salt,
        header.memory_kib,
        header.iterations,
        header.lanes,
    )?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key.as_ref()));
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(&header.nonce),
            Payload {
                msg: &container[HEADER_LEN..],
                aad: &container[..HEADER_LEN],
            },
        )
        .map_err(|_| Error::DecryptionFailed)?;
    let plaintext = Zeroizing::new(plaintext);
    let mut payload: SensitivePayload =
        serde_json::from_slice(plaintext.as_ref()).map_err(|_| Error::DecryptionFailed)?;
    if payload.metadata.id != header.note_id {
        return Err(Error::DecryptionFailed);
    }
    let note = Note {
        metadata: payload.metadata.clone(),
        body: payload.body.clone(),
        relative_path,
    };
    payload.clear();
    let session = EncryptedSession {
        key,
        salt: header.salt,
        memory_kib: header.memory_kib,
        iterations: header.iterations,
        lanes: header.lanes,
    };
    Ok((note, session))
}

pub fn inspect_header(container: &[u8]) -> Result<EncryptedHeader> {
    if container.len() < HEADER_LEN || &container[..MAGIC.len()] != MAGIC {
        return Err(Error::InvalidEncryptedNote(
            "missing SenatorialNotes encrypted-note header".into(),
        ));
    }
    let version = read_u16(container, 8)?;
    if version != FORMAT_VERSION {
        return Err(Error::InvalidEncryptedNote(format!(
            "unsupported format version {version}"
        )));
    }
    let flags = read_u16(container, 10)?;
    if flags != 0 {
        return Err(Error::InvalidEncryptedNote(
            "unsupported encrypted-note flags".into(),
        ));
    }
    let note_id = Uuid::from_slice(&container[12..28])
        .map_err(|error| Error::InvalidEncryptedNote(error.to_string()))?;
    let memory_kib = read_u32(container, 28)?;
    let iterations = read_u32(container, 32)?;
    let lanes = read_u32(container, 36)?;
    validate_kdf_parameters(memory_kib, iterations, lanes)?;
    let mut salt = [0_u8; SALT_LEN];
    salt.copy_from_slice(&container[40..56]);
    let mut nonce = [0_u8; NONCE_LEN];
    nonce.copy_from_slice(&container[56..80]);
    let ciphertext_len = read_u64(container, 80)?;
    let actual = container.len() - HEADER_LEN;
    if ciphertext_len != actual as u64 || actual < 16 {
        return Err(Error::InvalidEncryptedNote(
            "ciphertext length does not match the container".into(),
        ));
    }
    Ok(EncryptedHeader {
        note_id,
        memory_kib,
        iterations,
        lanes,
        salt,
        nonce,
        ciphertext_len,
    })
}

/// Argon2id KDF. Shared with the whole-vault engine (`crypto::vault`) so both
/// use identical parameters and the same conservative bounds check; the
/// `.snote` container behaviour is unchanged.
pub(crate) fn derive_key(
    password: &str,
    salt: &[u8; SALT_LEN],
    memory_kib: u32,
    iterations: u32,
    lanes: u32,
) -> Result<Zeroizing<[u8; KEY_LEN]>> {
    validate_kdf_parameters(memory_kib, iterations, lanes)?;
    let params = Params::new(memory_kib, iterations, lanes, Some(KEY_LEN))
        .map_err(|error| Error::Encryption(error.to_string()))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0_u8; KEY_LEN]);
    argon2
        .hash_password_into(password.as_bytes(), salt, key.as_mut())
        .map_err(|error| Error::Encryption(error.to_string()))?;
    Ok(key)
}

pub(crate) fn validate_kdf_parameters(memory_kib: u32, iterations: u32, lanes: u32) -> Result<()> {
    // Reject malicious containers that could request unreasonable work before
    // any allocation/derivation takes place.
    if !(8 * 1_024..=1_048_576).contains(&memory_kib)
        || !(1..=10).contains(&iterations)
        || !(1..=16).contains(&lanes)
    {
        return Err(Error::InvalidEncryptedNote(
            "KDF parameters are outside supported safety limits".into(),
        ));
    }
    Ok(())
}

fn encode_header(header: &EncryptedHeader) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_LEN);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
    bytes.extend_from_slice(&0_u16.to_be_bytes());
    bytes.extend_from_slice(header.note_id.as_bytes());
    bytes.extend_from_slice(&header.memory_kib.to_be_bytes());
    bytes.extend_from_slice(&header.iterations.to_be_bytes());
    bytes.extend_from_slice(&header.lanes.to_be_bytes());
    bytes.extend_from_slice(&header.salt);
    bytes.extend_from_slice(&header.nonce);
    bytes.extend_from_slice(&header.ciphertext_len.to_be_bytes());
    bytes
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| Error::InvalidEncryptedNote("truncated header".into()))?;
    Ok(u16::from_be_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| Error::InvalidEncryptedNote("truncated header".into()))?;
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| Error::InvalidEncryptedNote("truncated header".into()))?;
    Ok(u64::from_be_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}
