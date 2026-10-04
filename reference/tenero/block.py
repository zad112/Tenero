import hashlib, json, time


class Block:
    def __init__(self, index, transactions, previous_hash, timestamp=None, nonce=0, mix=""):
        self.index = index
        self.timestamp = time.time() if timestamp is None else timestamp
        self.transactions = transactions  # list of Transaction objects
        self.previous_hash = previous_hash
        self.nonce = nonce
        self.mix = mix  # matmul chains: the fold sums (64 bytes, hex) that the hash commits to
        self.hash = self.compute_hash()

    def _header_bytes(self):
        # everything except the nonce; built once so mining doesn't re-serialize
        return json.dumps({
            "index": self.index,
            "timestamp": self.timestamp,
            "transactions": [t.to_dict() for t in self.transactions],
            "previous_hash": self.previous_hash,
        }, sort_keys=True).encode()

    def compute_hash(self):
        return hashlib.sha256(self._header_bytes() + str(self.nonce).encode()).hexdigest()

    def meets_target(self, target):
        return int(self.hash, 16) < target

    def mine(self, target, refresh_timestamp=False, min_timestamp=0):
        # refresh_timestamp: keep the header's timestamp equal to the current time
        # while searching, so it records when the block was FOUND (the difficulty
        # adjustment measures block times from these timestamps)
        if refresh_timestamp:
            self.timestamp = max(int(time.time()), min_timestamp)
        base = hashlib.sha256(self._header_bytes())  # pre-hash the fixed part
        nonce = self.nonce
        tries = 0
        while True:
            h = base.copy()
            h.update(str(nonce).encode())
            digest = h.hexdigest()
            if int(digest, 16) < target:
                self.nonce = nonce
                self.hash = digest
                return
            nonce += 1
            if refresh_timestamp:
                tries += 1
                if tries >= 50_000:  # roughly every 50 ms
                    tries = 0
                    now = max(int(time.time()), min_timestamp)
                    if now != self.timestamp:
                        self.timestamp = now
                        base = hashlib.sha256(self._header_bytes())

    def to_dict(self):
        d = {
            "index": self.index,
            "timestamp": self.timestamp,
            "transactions": [t.to_dict() for t in self.transactions],
            "previous_hash": self.previous_hash,
            "nonce": self.nonce,
            "hash": self.hash,
        }
        if self.mix:
            d["mix"] = self.mix
        return d

    @classmethod
    def from_dict(cls, d):
        from .transaction import Transaction
        txs = [Transaction.from_dict(t) for t in d["transactions"]]
        block = cls(d["index"], txs, d["previous_hash"],
                    timestamp=d["timestamp"], nonce=d["nonce"], mix=d.get("mix", ""))
        if "hash" in d:
            block.hash = d["hash"]  # keep what was saved, so tampering is caught, not hidden
        return block
