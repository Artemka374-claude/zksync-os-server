from __future__ import annotations

import hashlib
from typing import Dict, List, Tuple

from genesis.account_properties import AccountProperties, ACCOUNT_PROPERTIES_STORAGE_ADDRESS
from genesis.tree import merkle_root

def derive_flat_storage_key(address_bytes: bytes, account_key: bytes) -> bytes:
    hasher = hashlib.blake2s(digest_size=32)
    buf = bytearray(32)
    buf[12:] = address_bytes
    hasher.update(buf)
    hasher.update(account_key)
    return hasher.digest()


def parse_bytes32(value: str) -> bytes:
    value = value[2:] if value.startswith("0x") else value
    if len(value) % 2:
        value = "0" + value
    return bytes.fromhex(value)


def to_account_key(address: bytes) -> bytes:
    key = bytearray(32)
    key[12:] = address
    return bytes(key)


def prepare_genesis(initial_contracts: Dict[str, str], additional_storage: Dict[str, str]) -> str:
    storage: Dict[bytes, bytes] = {}
    preimages: List[Tuple[bytes, bytes]] = []

    for address_hex, code_hex in initial_contracts.items():
        address = parse_bytes32(address_hex)[-20:]
        code = parse_bytes32(code_hex)

        account = AccountProperties()
        account.nonce = 1
        bytecode_preimage = account.set_properties_code(code)
        account_hash = account.compute_hash()

        flat_key = derive_flat_storage_key(
            ACCOUNT_PROPERTIES_STORAGE_ADDRESS,
            to_account_key(address),
        )
        if flat_key in storage:
            raise ValueError(f"duplicate storage key {flat_key.hex()}")

        storage[flat_key] = account_hash
        preimages.append((account.bytecode_hash, bytecode_preimage))
        preimages.append((account_hash, account.encoding()))

    for key_hex, value_hex in additional_storage:
        key = parse_bytes32(key_hex)
        value = parse_bytes32(value_hex)
        if key in storage:
            raise ValueError(f"duplicate storage key {key.hex()}")
        storage[key] = value

    ordered_logs = sorted(storage.items())

    genesis_root_hash, num_leaves = merkle_root(ordered_logs)
    genesis_commitment = genesis_batch_commitment(genesis_root_hash, num_leaves)
    return genesis_commitment

def genesis_batch_commitment(root_hash: bytes, num_leaves: int) -> str:
    last_256_blocks_hash = hashlib.blake2s(digest_size=32)
    for _ in range(255):
        last_256_blocks_hash.update(b"\x00" * 32)

    # Considered to be a constant, we expect no txs in there.
    genesis_block_hash = parse_bytes32("0xef97917ce9bd9fa7d12ad6a8d81ea26c80ac1d727acc37cbb9799ae199788dc5")
    last_256_blocks_hash.update(genesis_block_hash)
    last_256_blocks_hash = last_256_blocks_hash.digest()
    
    number = 0
    timestamp = 0

    hasher = hashlib.blake2s(digest_size=32)
    hasher.update(root_hash)
    hasher.update(num_leaves.to_bytes(8, "big"))
    hasher.update(number.to_bytes(8, "big"))
    hasher.update(last_256_blocks_hash)
    hasher.update(timestamp.to_bytes(8, "big"))
    # Encode as 0x-prefixed hex string
    return "0x" + hasher.digest().hex()

def genesis_object(genesis_commitment: str, initial_contracts: Dict[str, str], additional_storage: Dict[str, str]) -> Dict:
    state = {
        "initial_contracts": [[k, v] for k, v in initial_contracts.items()],
        "additional_storage": [[k, v] for k, v in additional_storage.items()],
        "execution_version": 3, # hardcoded for now
        "genesis_root": genesis_commitment,
    }
    return state
