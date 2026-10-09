# Test fixtures

Static inputs for the Rust tests. Keep them small, deterministic and free of real
key material; every file here is safe to publish.

| File | Used by | What it is |
|---|---|---|
| `fake_zfs.sh`, `fake_zpool.sh` | `tests/zfs_backend/support.rs` | Fake `zfs`/`zpool` executables. The loader fills in their placeholders and installs them per scenario; each script's header describes its protocol. |
| `gpg_expired_key.colons` | `src/crypto/gpg.rs` unit tests | `gpg --with-colons` listing of an expired key, so it is not encryption-capable. Restore must still find it by fingerprint (historical recipients); backup must refuse it. |
| `gpg_no_encrypt_key.colons` | `src/crypto/gpg.rs` unit tests | Listing of a valid key that can only sign and certify; same expectations as the expired key. |
| `tink_vector.hex` | `tests/crypto_stream.rs`, `src/crypto/stream.rs`, `tests/tooling/tink_interop.py` | Ciphertext produced by `tink_reference.c`, an independent OpenSSL implementation of Tink's raw `AES128_GCM_HKDF_1MB` streaming format. |
| `tink_runtime_vector.hex` | `tests/crypto_stream.rs`, `tests/tooling/tink_interop.py` | Ciphertext produced by the official Tink Python runtime for the same key, AAD and payload. |
| `tink_reference.c` | regenerating `tink_vector.hex` | The reference encryptor; build instructions are in its header. |

The GPG listings use invented fingerprints and key IDs. Both Tink vectors use the key
`00 01 … 1f`, AAD `tink cross-language vector` and payload
`Tink-compatible reference payload`. The tests, `tink_reference.c` and
`tests/tooling/tink_interop.py` must agree on these values, so change them together.
