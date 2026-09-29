import json

from .units import fmt
from .wallet import address_from_pubkey_hex, verify_signature

COINBASE = "COINBASE"  # special sender for block rewards


class Transaction:
    # amount and fee are whole numbers of the smallest unit (0.0001 coins)
    def __init__(self, sender, recipient, amount, fee=0, public_key="",
                 signature="", memo=""):
        self.sender = sender          # an address (or COINBASE)
        self.recipient = recipient    # an address
        self.amount = amount
        self.fee = fee                # paid to the miner, on top of amount
        self.public_key = public_key  # sender's public key (hex)
        self.signature = signature    # signature over the payload (hex)
        self.memo = memo              # short text note, covered by the signature

    def payload(self):
        # what gets signed: everything except the signature itself
        return json.dumps({
            "sender": self.sender,
            "recipient": self.recipient,
            "amount": self.amount,
            "fee": self.fee,
            "memo": self.memo,
            "public_key": self.public_key,
        }, sort_keys=True).encode()

    def sign(self, wallet):
        self.public_key = wallet.public_key
        self.signature = wallet.sign(self.payload())

    def size(self):
        # size in bytes of the serialized transaction. An unsigned transaction is
        # measured with placeholder key/signature, so the size is the same before
        # and after signing.
        d = self.to_dict()
        d["public_key"] = self.public_key or "0" * 128
        d["signature"] = self.signature or "0" * 128
        return len(json.dumps(d, sort_keys=True, separators=(",", ":")).encode())

    def is_signature_valid(self):
        if self.sender == COINBASE:
            return True  # block rewards are unsigned
        if not self.public_key or not self.signature:
            return False
        try:
            # the public key must actually belong to the claimed sender address
            if address_from_pubkey_hex(self.public_key) != self.sender:
                return False
        except ValueError:
            return False
        return verify_signature(self.public_key, self.payload(), self.signature)

    def to_dict(self):
        return {
            "sender": self.sender,
            "recipient": self.recipient,
            "amount": self.amount,
            "fee": self.fee,
            "memo": self.memo,
            "public_key": self.public_key,
            "signature": self.signature,
        }

    @classmethod
    def from_dict(cls, d):
        return cls(d["sender"], d["recipient"], d["amount"],
                   d.get("fee", 0), d.get("public_key", ""),
                   d.get("signature", ""), d.get("memo", ""))

    def __repr__(self):
        def short(a):
            return a if a == COINBASE else a[:8] + ".."
        if self.sender == COINBASE:
            return f"{COINBASE} -> {short(self.recipient)}: {fmt(self.amount)}"
        text = (f"{short(self.sender)} -> {short(self.recipient)}: "
                f"{fmt(self.amount)} (fee {fmt(self.fee)})")
        if self.memo:
            text += f'  "{self.memo}"'
        return text
