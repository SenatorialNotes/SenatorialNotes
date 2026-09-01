# SenatorialNotes Encrypted Note Format

This document describes `.snote` format version 1. It is a reviewable container around established cryptographic primitives, not a new cryptographic construction.

## Security model

The title, body, tags, and note-specific private metadata are encrypted at rest. Unlocking derives the correct key from the user's password. There is deliberately no password recovery, master key, backdoor, stored password, or password hint. Security also depends on password strength, implementation correctness, and the integrity of the running computer.

An unlocked computer, malware, a keylogger, screen capture, swap, or a process with sufficient access may obtain plaintext while a note is open. Full-disk encryption remains recommended for ordinary Markdown notes and broader system data.

## Primitives and parameters

- Password KDF: Argon2id, version 1.3.
- Memory cost: 65,536 KiB (64 MiB).
- Time cost: 3 iterations.
- Parallelism: 1 lane.
- Derived-key length: 32 bytes.
- Salt: 16 bytes from the operating system's cryptographically secure random generator.
- Authenticated encryption: XChaCha20-Poly1305.
- Nonce: 24 fresh random bytes for every encryption/save.
- Authentication tag: 16 bytes, included in the ciphertext output.

KDF parameters are stored in the header so future versions can migrate them. The reader rejects parameters outside conservative safety bounds before attempting derivation.

## Binary layout

All integer fields use big-endian byte order. The fixed header is 88 bytes:

| Offset | Length | Field |
| ---: | ---: | --- |
| 0 | 8 | Magic: `SNOTE\0\0\0` |
| 8 | 2 | Format version (`1`) |
| 10 | 2 | Flags (`0`) |
| 12 | 16 | Stable note UUID |
| 28 | 4 | Argon2 memory cost in KiB |
| 32 | 4 | Argon2 iteration count |
| 36 | 4 | Argon2 lanes |
| 40 | 16 | Random salt |
| 56 | 24 | Random XChaCha20 nonce |
| 80 | 8 | Ciphertext length |
| 88 | variable | Ciphertext plus Poly1305 tag |

The complete 88-byte header is authenticated as AEAD associated data. Modifying the UUID, KDF parameters, salt, nonce, flags, version, or ciphertext length therefore causes authentication failure.

The UUID remains clear so locked notes can be tracked without decrypting sensitive fields. The container also exposes its size, filesystem location/notebook, timestamps supplied by the filesystem, and the fact that it is an encrypted SenatorialNotes note. It does not expose the actual title.

## Encrypted payload

Version 1 serializes a UTF-8 JSON object containing:

- the complete note metadata object, including title, tags, timestamps, pin state, and preserved unknown/private fields;
- the Markdown body.

The serialized bytes are encrypted in full. The neutral filename is `encrypted--<short-uuid>.snote`.

## Writes and locking

Every save uses a fresh nonce and atomic sibling-file replacement. Initial conversion overwrites the original Markdown path with ciphertext before renaming it to `.snote`, avoiding a second plaintext copy. Encrypted notes never use plaintext crash-recovery files and locked content is not stored in persistent search data.

The derived key is retained only in an in-process session while permitted by the user's locking settings. Key and serialization buffers use established zeroization support where practical. Locking clears the editor and drops session key material; normal application exit always drops it.
