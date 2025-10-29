from __future__ import annotations

from dataclasses import dataclass
import hashlib

from genesis.keccak import keccak256

# ---------- AccountProperties mirroring basic_system ----------

ACCOUNT_PROPERTIES_STORAGE_ADDRESS = (0x8003).to_bytes(20, "big")
ARTIFACTS_CACHING_CODE_VERSION = 1
EXECUTION_ENVIRONMENT_EVM = 1
EIP7702_DELEGATION_PREFIX = bytes([0xEF, 0x01, 0x00])


def bytecode_padding_len(length: int) -> int:
    rem = length % 8
    return 0 if rem == 0 else 8 - rem


def build_jumpdest_bitmap(code: bytes) -> bytes:
    words = (len(code) + 63) // 64
    bitmap = [0] * words
    i = 0
    while i < len(code):
        op = code[i]
        if op == 0x5B:  # JUMPDEST
            idx = i // 64
            bit = i % 64
            bitmap[idx] |= 1 << bit
            i += 1
        elif 0x60 <= op <= 0x7F:  # PUSH1..PUSH32
            push_len = op - 0x5F
            i += 1 + push_len
        else:
            i += 1
    return b"".join(word.to_bytes(8, "little") for word in bitmap)


@dataclass
class VersioningData:
    value: int = 0

    def set_as_deployed(self) -> None:
        self.value = (self.value & 0x00FF_FFFF_FFFF_FFFF) | (1 << 56)

    def set_as_delegated(self) -> None:
        self.value = (self.value & 0x00FF_FFFF_FFFF_FFFF) | (2 << 56)

    def set_ee_version(self, value: int) -> None:
        self.value = (self.value & 0xFF00_FFFF_FFFF_FFFF) | (value << 48)

    def set_code_version(self, value: int) -> None:
        self.value = (self.value & 0xFFFF_00FF_FFFF_FFFF) | (value << 40)

    def to_bytes(self) -> bytes:
        return (self.value & 0xFFFFFFFFFFFFFFFF).to_bytes(8, "big")


@dataclass
class AccountProperties:
    versioning_data: VersioningData = VersioningData()
    nonce: int = 0
    balance: int = 0
    bytecode_hash: bytes = b"\x00" * 32
    unpadded_code_len: int = 0
    artifacts_len: int = 0
    observable_bytecode_hash: bytes = b"\x00" * 32
    observable_bytecode_len: int = 0

    def encoding(self) -> bytes:
        return (
            self.versioning_data.to_bytes()
            + self.nonce.to_bytes(8, "big")
            + self.balance.to_bytes(32, "big")
            + self.bytecode_hash
            + self.unpadded_code_len.to_bytes(4, "big")
            + self.artifacts_len.to_bytes(4, "big")
            + self.observable_bytecode_hash
            + self.observable_bytecode_len.to_bytes(4, "big")
        )

    def compute_hash(self) -> bytes:
        hasher = hashlib.blake2s(digest_size=32)
        hasher.update(self.versioning_data.to_bytes())
        hasher.update(self.nonce.to_bytes(8, "big"))
        hasher.update(self.balance.to_bytes(32, "big"))
        hasher.update(self.bytecode_hash)
        hasher.update(self.unpadded_code_len.to_bytes(4, "big"))
        hasher.update(self.artifacts_len.to_bytes(4, "big"))
        hasher.update(self.observable_bytecode_hash)
        hasher.update(self.observable_bytecode_len.to_bytes(4, "big"))
        return hasher.digest()


    def set_properties_code(self, evm_code: bytes) -> bytes:
        self.observable_bytecode_hash = keccak256(evm_code)
        self.unpadded_code_len = len(evm_code)
        self.observable_bytecode_len = len(evm_code)

        if len(evm_code) >= 3 and evm_code[:3] == EIP7702_DELEGATION_PREFIX:
            padding = bytecode_padding_len(len(evm_code))
            full_len = len(evm_code) + padding
            full = bytearray(full_len)
            full[: len(evm_code)] = evm_code
            self.bytecode_hash = hashlib.blake2s(full, digest_size=32).digest()
            self.artifacts_len = 0
            self.versioning_data.set_as_delegated()
            full_bytecode = bytes(full)
        else:
            artifacts = build_jumpdest_bitmap(evm_code)
            padding = bytecode_padding_len(len(evm_code))
            full_len = len(evm_code) + padding + len(artifacts)
            full = bytearray(full_len)
            full[: len(evm_code)] = evm_code
            full[len(evm_code) + padding :] = artifacts
            self.bytecode_hash = hashlib.blake2s(full, digest_size=32).digest()
            self.artifacts_len = len(artifacts)
            self.versioning_data.set_code_version(ARTIFACTS_CACHING_CODE_VERSION)
            self.versioning_data.set_as_deployed()
            full_bytecode = bytes(full)

        self.versioning_data.set_ee_version(EXECUTION_ENVIRONMENT_EVM)
        return full_bytecode
