#!/usr/bin/env python3
"""Independent E2EE v2 vector (PyNaCl + cryptography, not the Rust crates).

v2: Ed25519->X25519, ephemeral X25519 ECDH, HKDF-SHA256
(no salt, info b"ed25519_encryption", 32 bytes), XChaCha20-Poly1305
(24-byte nonce, empty AAD). Wire field is hex(epk32 || nonce24 || ct||tag).
"""

import hashlib
import json
from pathlib import Path

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.kdf.hkdf import HKDF
from nacl.bindings import (
    crypto_aead_xchacha20poly1305_ietf_encrypt,
    crypto_scalarmult,
    crypto_scalarmult_base,
    crypto_sign_ed25519_pk_to_curve25519,
    crypto_sign_ed25519_sk_to_curve25519,
    crypto_sign_seed_keypair,
)


def main() -> None:
    seed = bytes(range(32))
    pk, sk64 = crypto_sign_seed_keypair(seed)
    recipient_x = crypto_sign_ed25519_pk_to_curve25519(pk)
    # Cross-check the secret conversion the proxy uses (SHA-512 clamp).
    digest = hashlib.sha512(seed).digest()
    clamped = bytearray(digest[:32])
    clamped[0] &= 248
    clamped[31] &= 127
    clamped[31] |= 64
    assert bytes(clamped) == crypto_sign_ed25519_sk_to_curve25519(sk64)

    eph_sk = bytes((i * 7 + 3) % 256 for i in range(32))
    eph_pk = crypto_scalarmult_base(eph_sk)
    shared = crypto_scalarmult(eph_sk, recipient_x)
    key = HKDF(
        algorithm=hashes.SHA256(),
        length=32,
        salt=None,
        info=b"ed25519_encryption",
    ).derive(shared)
    nonce = bytes(range(24))
    plaintext = b"orbio-e2ee-v2-canary"
    ct = crypto_aead_xchacha20poly1305_ietf_encrypt(plaintext, b"", nonce, key)
    wire = eph_pk + nonce + ct
    out = {
        "plaintext": plaintext.decode(),
        "recipient_ed25519_seed": seed.hex(),
        "recipient_ed25519_public": pk.hex(),
        "ephemeral_x25519_secret": eph_sk.hex(),
        "ephemeral_x25519_public": eph_pk.hex(),
        "symmetric_key_hex": key.hex(),
        "nonce_hex": nonce.hex(),
        "ciphertext_hex": wire.hex(),
        "aad": "",
        "hkdf_info": "ed25519_encryption",
    }
    dest = Path(__file__).with_name("e2ee_v2.json")
    dest.write_text(json.dumps(out, indent=2) + "\n")
    print(dest)


if __name__ == "__main__":
    main()
