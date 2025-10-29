from __future__ import annotations

import hashlib
from typing import Dict, Iterable, List, Tuple

_empty_hashes: List[bytes] | None = None
def merkle_root(entries: Iterable[Tuple[bytes, bytes]]) -> Tuple[bytes, int]:
    TREE_DEPTH = 64
    MIN_GUARD_KEY = bytes(32)
    MAX_GUARD_KEY = bytes([0xFF]) * 32
    ZERO_VALUE = bytes(32)

    def hash_leaf(key: bytes, value: bytes, next_index: int) -> bytes:
        payload = key + value + next_index.to_bytes(8, "little")
        return hashlib.blake2s(payload, digest_size=32).digest()


    def hash_branch(lhs: bytes, rhs: bytes) -> bytes:
        return hashlib.blake2s(lhs + rhs, digest_size=32).digest()


    def empty_subtree_hash(depth: int) -> bytes:
        global _empty_hashes
        if _empty_hashes is None:
            zero_leaf = hash_leaf(bytes(32), bytes(32), 0)
            cache = [zero_leaf]
            for _ in range(TREE_DEPTH):
                cache.append(hash_branch(cache[-1], cache[-1]))
            _empty_hashes = cache
        return _empty_hashes[depth]

    # entries must already be in the same order the tree ingests them
    normalized = list(entries)

    index_map: Dict[bytes, int] = {MIN_GUARD_KEY: 0, MAX_GUARD_KEY: 1}
    for idx, (key, _) in enumerate(normalized, start=2):
        if key in index_map:
            raise ValueError(f"duplicate key {key.hex()}")
        index_map[key] = idx

    sorted_items = sorted(index_map.items(), key=lambda kv: kv[0])
    followers = [idx for _, idx in sorted_items][1:] + [1]
    next_index = {key: followers[i] for i, (key, _) in enumerate(sorted_items)}

    leaves = [
        (MIN_GUARD_KEY, ZERO_VALUE, next_index[MIN_GUARD_KEY]),
        (MAX_GUARD_KEY, ZERO_VALUE, next_index[MAX_GUARD_KEY]),
    ]
    leaves.extend((key, value, next_index[key]) for key, value in normalized)

    layer = [hash_leaf(k, v, nxt) for k, v, nxt in leaves]
    for depth in range(TREE_DEPTH):
        if len(layer) % 2 == 1:
            layer.append(empty_subtree_hash(depth))
        layer = [hash_branch(layer[i], layer[i + 1]) for i in range(0, len(layer), 2)]
    return layer[0], len(leaves)


# ---------- Demo ----------

# if __name__ == "__main__":
#     logs, preimages, raw = prepare_genesis(Path("/home/popzxc/workspace/zksync-os-server/genesis/genesis.json"))
#     print(f"{len(logs)} storage entries, {len(preimages)} preimages")

#     root_hash, leaf_count = merkle_root(logs)
#     print(f"Merkle root  : 0x{root_hash.hex()}")
#     print(f"Leaf count   : {leaf_count}")
#     print(f"Genesis root : {raw['genesis_root']} (state commitment)")
