//! Whole-vault encryption engine — v0.3 Stage D.
//!
//! Reviewable container design around Argon2id + XChaCha20-Poly1305 (already in
//! the tree, via [`crypto::note`](super::note)) plus HKDF-SHA256 for
//! domain-separated subkeys. Invents no cryptographic construction. The full
//! format is `docs/ENCRYPTED_VAULT_FORMAT.md`.
//!
//! Key hierarchy:
//! ```text
//! password --Argon2id(salt, params)--> KEK --unwrap--> VMK (32 random bytes)
//!                                                        |
//!                          HKDF-SHA256(ikm=VMK, salt=vault_id, info=<label>)
//!                                                        |
//!            k_content   k_names   k_attachments   k_metadata   k_index
//! ```
//! A password change re-wraps the VMK only; no object blob is re-encrypted.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::note::{
    ARGON2_ITERATIONS, ARGON2_LANES, ARGON2_MEMORY_KIB, KEY_LEN, NONCE_LEN, SALT_LEN, derive_key,
    validate_kdf_parameters,
};
use crate::{Error, Result};

// --- keyfile (`.senatorial-notes/vault.keys`) ------------------------------

const KEYFILE_MAGIC: &[u8; 8] = b"SNVLT\0\0\0";
const KEYFILE_VERSION: u16 = 1;
const KEYFILE_HEADER_LEN: usize = 88;
const TAG_LEN: usize = 16;
const WRAPPED_VMK_LEN: usize = KEY_LEN + TAG_LEN; // 48

// --- SNENC object container ------------------------------------------------

const OBJECT_MAGIC: &[u8; 8] = b"SNENC\0\0\0";
const OBJECT_VERSION: u16 = 1;
/// magic(8) ver(2) flags(2) vault_id(16) object_uuid(16) type(2) reserved(2)
/// ciphertext_len(8) nonce(24) = 80.
const OBJECT_HEADER_LEN: usize = 80;

/// `flags` bit 0: the plaintext of this blob is itself a format-version-1
/// `.snote` container (see `docs/ENCRYPTED_VAULT_FORMAT.md` §8).
const FLAG_INNER_SNOTE: u16 = 1;

// --- HKDF domain-separation labels — FORMAT-STABLE -------------------------
//
// Changing any of these is a format-version bump, never a silent edit
// (`docs/ENCRYPTED_VAULT_FORMAT.md` §3.1; enforced by
// `encrypted_vault::hkdf_labels_match_format_doc`).

pub const HKDF_LABEL_CONTENT: &[u8] = b"senatorialnotes/vault/v1/content";
pub const HKDF_LABEL_NAMES: &[u8] = b"senatorialnotes/vault/v1/names";
pub const HKDF_LABEL_ATTACHMENTS: &[u8] = b"senatorialnotes/vault/v1/attachments";
pub const HKDF_LABEL_METADATA: &[u8] = b"senatorialnotes/vault/v1/metadata";
pub const HKDF_LABEL_INDEX: &[u8] = b"senatorialnotes/vault/v1/index";

/// What an [`SNENC`](self) blob holds. Bound into the AAD, so a blob of one
/// type can never be opened as another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectType {
    /// A plaintext-Markdown note (`Note::to_markdown` bytes).
    Note,
    /// A per-note `.snote` v1 container, wrapped as an inner layer (§8).
    InnerSnote,
    /// An attachment file's raw bytes.
    Attachment,
    /// The single encrypted vault manifest (notebook tree + object index +
    /// trash + attachment index).
    Manifest,
    /// Per-vault metadata / UI state.
    Metadata,
    /// Reserved for a future encrypted search index.
    Index,
}

impl ObjectType {
    fn code(self) -> u16 {
        match self {
            ObjectType::Note => 0,
            ObjectType::Attachment => 1,
            ObjectType::Manifest => 3,
            ObjectType::Metadata => 5,
            ObjectType::InnerSnote => 6,
            ObjectType::Index => 7,
        }
    }

    fn from_code(code: u16) -> Result<Self> {
        Ok(match code {
            0 => ObjectType::Note,
            1 => ObjectType::Attachment,
            3 => ObjectType::Manifest,
            5 => ObjectType::Metadata,
            6 => ObjectType::InnerSnote,
            7 => ObjectType::Index,
            other => {
                return Err(Error::InvalidEncryptedVault(format!(
                    "unknown object type {other}"
                )));
            }
        })
    }
}

/// The in-memory, unlocked key material for one vault. Every field is
/// `Zeroizing` and is cleared when this value is dropped (on Lock Now, auto-lock,
/// vault switch, or process exit).
pub struct VaultKeys {
    vault_id: Uuid,
    content: Zeroizing<[u8; KEY_LEN]>,
    names: Zeroizing<[u8; KEY_LEN]>,
    attachments: Zeroizing<[u8; KEY_LEN]>,
    metadata: Zeroizing<[u8; KEY_LEN]>,
    #[allow(dead_code)] // reserved for a future encrypted index
    index: Zeroizing<[u8; KEY_LEN]>,
}

impl std::fmt::Debug for VaultKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultKeys")
            .field("vault_id", &self.vault_id)
            .field("keys", &"[REDACTED]")
            .finish()
    }
}

impl VaultKeys {
    pub fn vault_id(&self) -> Uuid {
        self.vault_id
    }

    fn subkey(&self, object_type: ObjectType) -> &[u8; KEY_LEN] {
        match object_type {
            ObjectType::Note | ObjectType::InnerSnote => &self.content,
            ObjectType::Attachment => &self.attachments,
            ObjectType::Manifest => &self.names,
            ObjectType::Metadata => &self.metadata,
            ObjectType::Index => &self.index,
        }
    }

    /// Encrypts `plaintext` into an `SNENC` blob. `inner_snote` sets the
    /// `INNER_SNOTE` flag (the plaintext must be a valid `.snote` v1 container).
    pub fn seal(
        &self,
        object_type: ObjectType,
        object_uuid: Uuid,
        inner_snote: bool,
        plaintext: &[u8],
    ) -> Result<Vec<u8>> {
        let mut nonce = [0_u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|error| Error::Encryption(error.to_string()))?;

        let ciphertext_len = plaintext
            .len()
            .checked_add(TAG_LEN)
            .ok_or_else(|| Error::Encryption("object is too large".into()))?
            as u64;
        let flags = if inner_snote { FLAG_INNER_SNOTE } else { 0 };
        let header = object_header(
            self.vault_id,
            object_uuid,
            object_type,
            flags,
            nonce,
            ciphertext_len,
        );

        let cipher = XChaCha20Poly1305::new(Key::from_slice(self.subkey(object_type)));
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &header,
                },
            )
            .map_err(|_| Error::Encryption("authenticated encryption failed".into()))?;

        let mut blob = Vec::with_capacity(OBJECT_HEADER_LEN + ciphertext.len());
        blob.extend_from_slice(&header);
        blob.extend_from_slice(&ciphertext);
        Ok(blob)
    }

    /// Decrypts an `SNENC` blob, verifying that its bound identity
    /// (`vault_id`, `object_uuid`, `object_type`, container version) is exactly
    /// what the caller expects. Any structural or authentication problem is an
    /// error — unauthenticated plaintext is never returned.
    ///
    /// Returns `(plaintext, is_inner_snote)`.
    pub fn open(
        &self,
        object_type: ObjectType,
        expected_object_uuid: Uuid,
        blob: &[u8],
    ) -> Result<(Zeroizing<Vec<u8>>, bool)> {
        if blob.len() < OBJECT_HEADER_LEN || &blob[..OBJECT_MAGIC.len()] != OBJECT_MAGIC {
            return Err(Error::InvalidEncryptedVault(
                "missing SNENC object header".into(),
            ));
        }
        let version = read_u16(blob, 8);
        if version != OBJECT_VERSION {
            return Err(Error::InvalidEncryptedVault(format!(
                "unsupported object container version {version}"
            )));
        }
        let flags = read_u16(blob, 10);
        if flags & !FLAG_INNER_SNOTE != 0 {
            return Err(Error::InvalidEncryptedVault(
                "unsupported object flags".into(),
            ));
        }
        let vault_id = Uuid::from_slice(&blob[12..28])
            .map_err(|error| Error::InvalidEncryptedVault(error.to_string()))?;
        if vault_id != self.vault_id {
            return Err(Error::InvalidEncryptedVault(
                "object belongs to a different vault".into(),
            ));
        }
        let object_uuid = Uuid::from_slice(&blob[28..44])
            .map_err(|error| Error::InvalidEncryptedVault(error.to_string()))?;
        if object_uuid != expected_object_uuid {
            return Err(Error::InvalidEncryptedVault(
                "object identity does not match the manifest".into(),
            ));
        }
        if ObjectType::from_code(read_u16(blob, 44))? != object_type {
            return Err(Error::InvalidEncryptedVault(
                "object is not of the expected type".into(),
            ));
        }
        if read_u16(blob, 46) != 0 {
            return Err(Error::InvalidEncryptedVault(
                "reserved header bytes are not zero".into(),
            ));
        }
        let ciphertext_len = read_u64(blob, 48);
        let nonce: [u8; NONCE_LEN] = blob[56..80]
            .try_into()
            .map_err(|_| Error::InvalidEncryptedVault("truncated nonce".into()))?;
        let ciphertext = &blob[OBJECT_HEADER_LEN..];
        if ciphertext_len != ciphertext.len() as u64 || ciphertext.len() < TAG_LEN {
            return Err(Error::InvalidEncryptedVault(
                "ciphertext length does not match the container".into(),
            ));
        }

        let cipher = XChaCha20Poly1305::new(Key::from_slice(self.subkey(object_type)));
        let plaintext = cipher
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: ciphertext,
                    aad: &blob[..OBJECT_HEADER_LEN],
                },
            )
            .map_err(|_| Error::DecryptionFailed)?;
        Ok((Zeroizing::new(plaintext), flags & FLAG_INNER_SNOTE != 0))
    }
}

/// Creates a fresh vault key schedule and the `vault.keys` bytes that wrap it.
///
/// Returns `(keyfile_bytes, keys)`. The VMK is 32 random bytes; the caller's
/// `password` derives the KEK that wraps it.
pub fn create_keyfile(vault_id: Uuid, password: &str) -> Result<(Vec<u8>, VaultKeys)> {
    let mut vmk = Zeroizing::new([0_u8; KEY_LEN]);
    getrandom::fill(vmk.as_mut()).map_err(|error| Error::Encryption(error.to_string()))?;
    let keyfile = wrap_vmk(vault_id, &vmk, password)?;
    let keys = expand_keys(vault_id, &vmk)?;
    Ok((keyfile, keys))
}

/// Unwraps `vault.keys` with `password` and expands the HKDF subkeys.
///
/// Wrong password, a tampered header, or a tampered wrapped VMK all fail with
/// [`Error::DecryptionFailed`] without producing any key material.
pub fn open_keyfile(keyfile: &[u8], vault_id: Uuid, password: &str) -> Result<VaultKeys> {
    let (salt, memory_kib, iterations, lanes, wrap_nonce, header) =
        inspect_keyfile(keyfile, vault_id)?;
    let kek = derive_key(password, &salt, memory_kib, iterations, lanes)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(kek.as_ref()));
    let vmk_bytes = cipher
        .decrypt(
            XNonce::from_slice(&wrap_nonce),
            Payload {
                msg: &keyfile[KEYFILE_HEADER_LEN..],
                aad: &header,
            },
        )
        .map_err(|_| Error::DecryptionFailed)?;
    let mut vmk = Zeroizing::new([0_u8; KEY_LEN]);
    if vmk_bytes.len() != KEY_LEN {
        return Err(Error::DecryptionFailed);
    }
    vmk.copy_from_slice(&vmk_bytes);
    drop(Zeroizing::new(vmk_bytes));
    expand_keys(vault_id, &vmk)
}

/// Re-wraps the existing VMK under `new_password` (fresh salt + wrap nonce).
/// Verifies `old_password` first; **no object blob is re-encrypted**.
pub fn rewrap_keyfile(
    keyfile: &[u8],
    vault_id: Uuid,
    old_password: &str,
    new_password: &str,
) -> Result<Vec<u8>> {
    let (salt, memory_kib, iterations, lanes, wrap_nonce, header) =
        inspect_keyfile(keyfile, vault_id)?;
    let kek = derive_key(old_password, &salt, memory_kib, iterations, lanes)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(kek.as_ref()));
    let vmk_bytes = cipher
        .decrypt(
            XNonce::from_slice(&wrap_nonce),
            Payload {
                msg: &keyfile[KEYFILE_HEADER_LEN..],
                aad: &header,
            },
        )
        .map_err(|_| Error::DecryptionFailed)?;
    if vmk_bytes.len() != KEY_LEN {
        return Err(Error::DecryptionFailed);
    }
    let mut vmk = Zeroizing::new([0_u8; KEY_LEN]);
    vmk.copy_from_slice(&vmk_bytes);
    drop(Zeroizing::new(vmk_bytes));
    wrap_vmk(vault_id, &vmk, new_password)
}

fn wrap_vmk(vault_id: Uuid, vmk: &[u8; KEY_LEN], password: &str) -> Result<Vec<u8>> {
    let mut salt = [0_u8; SALT_LEN];
    getrandom::fill(&mut salt).map_err(|error| Error::Encryption(error.to_string()))?;
    let mut wrap_nonce = [0_u8; NONCE_LEN];
    getrandom::fill(&mut wrap_nonce).map_err(|error| Error::Encryption(error.to_string()))?;

    let header = keyfile_header(
        vault_id,
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_LANES,
        salt,
        wrap_nonce,
    );
    let kek = derive_key(
        password,
        &salt,
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_LANES,
    )?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(kek.as_ref()));
    let wrapped = cipher
        .encrypt(
            XNonce::from_slice(&wrap_nonce),
            Payload {
                msg: vmk.as_slice(),
                aad: &header,
            },
        )
        .map_err(|_| Error::Encryption("VMK wrap failed".into()))?;

    let mut out = Vec::with_capacity(KEYFILE_HEADER_LEN + wrapped.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&wrapped);
    Ok(out)
}

#[allow(clippy::type_complexity)]
fn inspect_keyfile(
    keyfile: &[u8],
    vault_id: Uuid,
) -> Result<([u8; SALT_LEN], u32, u32, u32, [u8; NONCE_LEN], Vec<u8>)> {
    if keyfile.len() < KEYFILE_HEADER_LEN || &keyfile[..KEYFILE_MAGIC.len()] != KEYFILE_MAGIC {
        return Err(Error::InvalidEncryptedVault(
            "missing SNVLT keyfile header".into(),
        ));
    }
    if read_u16(keyfile, 8) != KEYFILE_VERSION {
        return Err(Error::InvalidEncryptedVault(
            "unsupported keyfile version".into(),
        ));
    }
    if read_u16(keyfile, 10) != 0 {
        return Err(Error::InvalidEncryptedVault(
            "unsupported keyfile flags".into(),
        ));
    }
    let keyfile_vault_id = Uuid::from_slice(&keyfile[12..28])
        .map_err(|error| Error::InvalidEncryptedVault(error.to_string()))?;
    if keyfile_vault_id != vault_id {
        return Err(Error::InvalidEncryptedVault(
            "keyfile is for a different vault".into(),
        ));
    }
    let memory_kib = read_u32(keyfile, 28);
    let iterations = read_u32(keyfile, 32);
    let lanes = read_u32(keyfile, 36);
    validate_kdf_parameters(memory_kib, iterations, lanes)?;
    let salt: [u8; SALT_LEN] = keyfile[40..56].try_into().unwrap();
    let wrap_nonce: [u8; NONCE_LEN] = keyfile[56..80].try_into().unwrap();
    if read_u64(keyfile, 80) != WRAPPED_VMK_LEN as u64
        || keyfile.len() != KEYFILE_HEADER_LEN + WRAPPED_VMK_LEN
    {
        return Err(Error::InvalidEncryptedVault(
            "wrapped-VMK length does not match the keyfile".into(),
        ));
    }
    Ok((
        salt,
        memory_kib,
        iterations,
        lanes,
        wrap_nonce,
        keyfile[..KEYFILE_HEADER_LEN].to_vec(),
    ))
}

fn keyfile_header(
    vault_id: Uuid,
    memory_kib: u32,
    iterations: u32,
    lanes: u32,
    salt: [u8; SALT_LEN],
    wrap_nonce: [u8; NONCE_LEN],
) -> Vec<u8> {
    let mut header = Vec::with_capacity(KEYFILE_HEADER_LEN);
    header.extend_from_slice(KEYFILE_MAGIC);
    header.extend_from_slice(&KEYFILE_VERSION.to_be_bytes());
    header.extend_from_slice(&0_u16.to_be_bytes());
    header.extend_from_slice(vault_id.as_bytes());
    header.extend_from_slice(&memory_kib.to_be_bytes());
    header.extend_from_slice(&iterations.to_be_bytes());
    header.extend_from_slice(&lanes.to_be_bytes());
    header.extend_from_slice(&salt);
    header.extend_from_slice(&wrap_nonce);
    header.extend_from_slice(&(WRAPPED_VMK_LEN as u64).to_be_bytes());
    debug_assert_eq!(header.len(), KEYFILE_HEADER_LEN);
    header
}

fn object_header(
    vault_id: Uuid,
    object_uuid: Uuid,
    object_type: ObjectType,
    flags: u16,
    nonce: [u8; NONCE_LEN],
    ciphertext_len: u64,
) -> Vec<u8> {
    let mut header = Vec::with_capacity(OBJECT_HEADER_LEN);
    header.extend_from_slice(OBJECT_MAGIC);
    header.extend_from_slice(&OBJECT_VERSION.to_be_bytes());
    header.extend_from_slice(&flags.to_be_bytes());
    header.extend_from_slice(vault_id.as_bytes());
    header.extend_from_slice(object_uuid.as_bytes());
    header.extend_from_slice(&object_type.code().to_be_bytes());
    header.extend_from_slice(&0_u16.to_be_bytes());
    header.extend_from_slice(&ciphertext_len.to_be_bytes());
    header.extend_from_slice(&nonce);
    debug_assert_eq!(header.len(), OBJECT_HEADER_LEN);
    header
}

fn expand_keys(vault_id: Uuid, vmk: &[u8; KEY_LEN]) -> Result<VaultKeys> {
    let hkdf = Hkdf::<Sha256>::new(Some(vault_id.as_bytes()), vmk.as_slice());
    let subkey = |label: &[u8]| -> Result<Zeroizing<[u8; KEY_LEN]>> {
        let mut key = Zeroizing::new([0_u8; KEY_LEN]);
        hkdf.expand(label, key.as_mut())
            .map_err(|_| Error::Encryption("HKDF expand failed".into()))?;
        Ok(key)
    };
    Ok(VaultKeys {
        vault_id,
        content: subkey(HKDF_LABEL_CONTENT)?,
        names: subkey(HKDF_LABEL_NAMES)?,
        attachments: subkey(HKDF_LABEL_ATTACHMENTS)?,
        metadata: subkey(HKDF_LABEL_METADATA)?,
        index: subkey(HKDF_LABEL_INDEX)?,
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PW: &str = "correct horse battery staple";

    fn keys(vault_id: Uuid) -> VaultKeys {
        let (keyfile, keys) = create_keyfile(vault_id, PW).unwrap();
        assert_eq!(
            open_keyfile(&keyfile, vault_id, PW).unwrap().vault_id(),
            vault_id
        );
        keys
    }

    #[test]
    fn keyfile_round_trips_and_wrong_password_fails() {
        let vault_id = Uuid::new_v4();
        let (keyfile, _) = create_keyfile(vault_id, PW).unwrap();
        assert!(open_keyfile(&keyfile, vault_id, PW).is_ok());
        assert!(matches!(
            open_keyfile(&keyfile, vault_id, "wrong password here"),
            Err(Error::DecryptionFailed)
        ));
    }

    #[test]
    fn a_tampered_keyfile_byte_fails() {
        let vault_id = Uuid::new_v4();
        let (mut keyfile, _) = create_keyfile(vault_id, PW).unwrap();
        let last = keyfile.len() - 1;
        keyfile[last] ^= 0x01;
        assert!(matches!(
            open_keyfile(&keyfile, vault_id, PW),
            Err(Error::DecryptionFailed)
        ));
        // A header byte too.
        let (mut keyfile, _) = create_keyfile(vault_id, PW).unwrap();
        keyfile[45] ^= 0x01; // inside kdf salt
        assert!(open_keyfile(&keyfile, vault_id, PW).is_err());
    }

    #[test]
    fn keyfile_for_another_vault_is_rejected() {
        let (keyfile, _) = create_keyfile(Uuid::new_v4(), PW).unwrap();
        assert!(matches!(
            open_keyfile(&keyfile, Uuid::new_v4(), PW),
            Err(Error::InvalidEncryptedVault(_))
        ));
    }

    #[test]
    fn rewrap_keeps_the_same_subkeys() {
        let vault_id = Uuid::new_v4();
        let (keyfile, keys_before) = create_keyfile(vault_id, PW).unwrap();
        let blob = keys_before
            .seal(ObjectType::Note, Uuid::new_v4(), false, b"hello")
            .unwrap();

        let rewrapped = rewrap_keyfile(&keyfile, vault_id, PW, "a brand new passphrase!!").unwrap();
        assert!(open_keyfile(&rewrapped, vault_id, PW).is_err());
        let keys_after = open_keyfile(&rewrapped, vault_id, "a brand new passphrase!!").unwrap();

        // The blob sealed with the old keyfile still opens with the new one -
        // the VMK (and every subkey) is unchanged.
        let object_uuid = Uuid::from_slice(&blob[28..44]).unwrap();
        let (plain, _) = keys_after
            .open(ObjectType::Note, object_uuid, &blob)
            .unwrap();
        assert_eq!(&plain[..], b"hello");
    }

    #[test]
    fn rewrap_requires_the_old_password() {
        let vault_id = Uuid::new_v4();
        let (keyfile, _) = create_keyfile(vault_id, PW).unwrap();
        assert!(matches!(
            rewrap_keyfile(&keyfile, vault_id, "not the old one!!", "new one here!!"),
            Err(Error::DecryptionFailed)
        ));
    }

    #[test]
    fn seal_open_round_trip_and_tamper_fails() {
        let vault_id = Uuid::new_v4();
        let k = keys(vault_id);
        let id = Uuid::new_v4();
        let mut blob = k.seal(ObjectType::Note, id, false, b"secret body").unwrap();
        let (plain, inner) = k.open(ObjectType::Note, id, &blob).unwrap();
        assert_eq!(&plain[..], b"secret body");
        assert!(!inner);

        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        assert!(matches!(
            k.open(ObjectType::Note, id, &blob),
            Err(Error::DecryptionFailed)
        ));
    }

    #[test]
    fn cross_object_and_cross_type_substitution_fail() {
        let vault_id = Uuid::new_v4();
        let k = keys(vault_id);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let blob_a = k.seal(ObjectType::Note, a, false, b"a").unwrap();
        // Opening blob A while expecting object B fails.
        assert!(matches!(
            k.open(ObjectType::Note, b, &blob_a),
            Err(Error::InvalidEncryptedVault(_))
        ));
        // Opening a Note blob as an Attachment fails (different subkey + type AAD).
        assert!(k.open(ObjectType::Attachment, a, &blob_a).is_err());
    }

    #[test]
    fn cross_vault_substitution_fails() {
        let vault_a = Uuid::new_v4();
        let vault_b = Uuid::new_v4();
        let ka = keys(vault_a);
        let kb = keys(vault_b);
        let id = Uuid::new_v4();
        let blob = ka.seal(ObjectType::Note, id, false, b"x").unwrap();
        assert!(matches!(
            kb.open(ObjectType::Note, id, &blob),
            Err(Error::InvalidEncryptedVault(_))
        ));
    }

    #[test]
    fn hkdf_domain_separation_uses_five_distinct_keys() {
        let vault_id = Uuid::new_v4();
        let k = keys(vault_id);
        let all = [
            k.content.to_vec(),
            k.names.to_vec(),
            k.attachments.to_vec(),
            k.metadata.to_vec(),
            k.index.to_vec(),
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "subkeys {i} and {j} must differ");
            }
        }
        // And a Manifest (names) blob cannot be opened as a Note (content).
        let id = Uuid::new_v4();
        let manifest = k.seal(ObjectType::Manifest, id, false, b"tree").unwrap();
        assert!(k.open(ObjectType::Note, id, &manifest).is_err());
    }

    #[test]
    fn nonces_are_unique_across_seals() {
        let k = keys(Uuid::new_v4());
        let id = Uuid::new_v4();
        let mut nonces = std::collections::HashSet::new();
        for _ in 0..500 {
            let blob = k
                .seal(ObjectType::Note, id, false, b"same plaintext")
                .unwrap();
            assert!(nonces.insert(blob[56..80].to_vec()), "nonce reused");
        }
    }

    #[test]
    fn inner_snote_flag_round_trips() {
        let k = keys(Uuid::new_v4());
        let id = Uuid::new_v4();
        let blob = k
            .seal(ObjectType::InnerSnote, id, true, b"SNOTE\0\0\0...")
            .unwrap();
        let (plain, inner) = k.open(ObjectType::InnerSnote, id, &blob).unwrap();
        assert!(inner);
        assert_eq!(&plain[..], b"SNOTE\0\0\0...");
    }

    #[test]
    fn hkdf_labels_are_the_documented_bytes() {
        assert_eq!(HKDF_LABEL_CONTENT, b"senatorialnotes/vault/v1/content");
        assert_eq!(HKDF_LABEL_NAMES, b"senatorialnotes/vault/v1/names");
        assert_eq!(
            HKDF_LABEL_ATTACHMENTS,
            b"senatorialnotes/vault/v1/attachments"
        );
        assert_eq!(HKDF_LABEL_METADATA, b"senatorialnotes/vault/v1/metadata");
        assert_eq!(HKDF_LABEL_INDEX, b"senatorialnotes/vault/v1/index");
    }
}
