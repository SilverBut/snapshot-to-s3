#!/usr/bin/env python3
"""Cross-check snapshot-to-s3's Streaming AEAD framing against official Tink Python.

Requires the pinned ``tink`` package (see tests/provision/lib/hosted.sh).

* Default: decrypt both committed vectors in tests/fixtures and check that a
  fresh Tink encryption of the reference payload has the expected size.
* ``--round-trip``: used by ``crypto_stream::official_tink_runtime_bidirectional``.
  Decrypt Rust ciphertext from stdin, require the agreed 3 MiB + 17 byte
  pattern, and write Tink's re-encryption of it to stdout for Rust to decrypt.
"""

import argparse
import io
import sys
from pathlib import Path

import tink.streaming_aead as streaming_aead
from tink import core
from tink.proto import aes_gcm_hkdf_streaming_pb2, common_pb2, tink_pb2
from tink.streaming_aead import _raw_streaming_aead

FIXTURES = Path(__file__).resolve().parent.parent / "fixtures"
VECTORS = ("tink_vector.hex", "tink_runtime_vector.hex")
# AAD and the round-trip payload must match tests/crypto_stream.rs.
AAD = b"tink cross-language vector"
PLAINTEXT = b"Tink-compatible reference payload"
KEY = bytes(range(32))
SEGMENT_SIZE = 1_048_576
REFERENCE_CIPHERTEXT_LEN = 73
ROUND_TRIP_LEN = 3 * 1024 * 1024 + 17


class InteropError(Exception):
    pass


class RetainedBytesIO(io.BytesIO):
    """Keeps its contents readable after Tink closes the ciphertext sink."""

    def close(self) -> None:
        self.value_after_close = self.getvalue()
        super().close()


def primitive() -> _raw_streaming_aead.RawStreamingAead:
    streaming_aead.register()
    key = aes_gcm_hkdf_streaming_pb2.AesGcmHkdfStreamingKey(version=0, key_value=KEY)
    key.params.hkdf_hash_type = common_pb2.SHA256
    key.params.derived_key_size = 16
    key.params.ciphertext_segment_size = SEGMENT_SIZE
    key_data = tink_pb2.KeyData(
        type_url="type.googleapis.com/google.crypto.tink.AesGcmHkdfStreamingKey",
        value=key.SerializeToString(),
        key_material_type=tink_pb2.KeyData.SYMMETRIC,
    )
    return core.Registry.primitive(key_data, _raw_streaming_aead.RawStreamingAead)


def decrypt(aead, ciphertext: bytes) -> bytes:
    with aead.new_raw_decrypting_stream(
        io.BytesIO(ciphertext), AAD, close_ciphertext_source=False
    ) as plaintext:
        return plaintext.read()


def encrypt(aead, plaintext: bytes) -> bytes:
    sink = RetainedBytesIO()
    with aead.new_raw_encrypting_stream(sink, AAD) as encryptor:
        encryptor.write(plaintext)
    return sink.value_after_close


def require(condition: bool, message: str) -> None:
    if not condition:
        raise InteropError(message)


def check_vectors(aead) -> None:
    for name in VECTORS:
        ciphertext = bytes.fromhex((FIXTURES / name).read_text(encoding="ascii").strip())
        require(decrypt(aead, ciphertext) == PLAINTEXT, f"{name} did not decrypt to PLAINTEXT")
    generated = encrypt(aead, PLAINTEXT)
    require(decrypt(aead, generated) == PLAINTEXT, "Tink did not round-trip PLAINTEXT")
    require(
        len(generated) == REFERENCE_CIPHERTEXT_LEN,
        f"Tink ciphertext is {len(generated)} bytes, expected {REFERENCE_CIPHERTEXT_LEN}",
    )


def round_trip(aead) -> None:
    decrypted = decrypt(aead, sys.stdin.buffer.read())
    expected = bytes(index % 251 for index in range(ROUND_TRIP_LEN))
    require(decrypted == expected, "Rust ciphertext did not decrypt to the expected payload")
    sys.stdout.buffer.write(encrypt(aead, decrypted))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--round-trip", action="store_true", help="stdin/stdout mode for Rust")
    args = parser.parse_args()
    aead = primitive()
    try:
        round_trip(aead) if args.round_trip else check_vectors(aead)
    except InteropError as error:
        print(f"Tink interop failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
