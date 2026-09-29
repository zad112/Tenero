import hashlib
import json
import os

from ecdsa import SigningKey, VerifyingKey, SECP256k1, BadSignatureError


def address_from_pubkey_hex(pubkey_hex):
    # address = first 40 hex chars of SHA-256 of the public key
    return hashlib.sha256(bytes.fromhex(pubkey_hex)).hexdigest()[:40]


class Wallet:
    def __init__(self, private_key_hex=None):
        if private_key_hex:
            self.signing_key = SigningKey.from_string(
                bytes.fromhex(private_key_hex), curve=SECP256k1
            )
        else:
            self.signing_key = SigningKey.generate(curve=SECP256k1)
        self.verifying_key = self.signing_key.get_verifying_key()
        self.public_key = self.verifying_key.to_string().hex()
        self.address = address_from_pubkey_hex(self.public_key)

    def private_key_hex(self):
        return self.signing_key.to_string().hex()

    def sign(self, message_bytes):
        return self.signing_key.sign(message_bytes).hex()

    def save(self, path):
        with open(path, "w") as f:
            json.dump({"private_key": self.private_key_hex()}, f)

    @classmethod
    def load(cls, path):
        with open(path) as f:
            return cls(json.load(f)["private_key"])

    @classmethod
    def load_or_create(cls, path):
        if os.path.exists(path):
            return cls.load(path)
        w = cls()
        w.save(path)
        return w


def verify_signature(pubkey_hex, message_bytes, signature_hex):
    try:
        vk = VerifyingKey.from_string(bytes.fromhex(pubkey_hex), curve=SECP256k1)
        return vk.verify(bytes.fromhex(signature_hex), message_bytes)
    except (BadSignatureError, ValueError):
        return False
