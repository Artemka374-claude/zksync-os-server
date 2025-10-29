from __future__ import annotations

from typing import List

# Yes, I generated a keccack-256 implementation with an LLM to avoid dependencies.
# Whatchagonnado?
def keccak256(data: bytes) -> bytes:
    _MASK = 0xFFFFFFFFFFFFFFFF
    _RC = [
        0x0000000000000001,
        0x0000000000008082,
        0x800000000000808A,
        0x8000000080008000,
        0x000000000000808B,
        0x0000000080000001,
        0x8000000080008081,
        0x8000000000008009,
        0x000000000000008A,
        0x0000000000000088,
        0x0000000080008009,
        0x000000008000000A,
        0x000000008000808B,
        0x800000000000008B,
        0x8000000000008089,
        0x8000000000008003,
        0x8000000000008002,
        0x8000000000000080,
        0x000000000000800A,
        0x800000008000000A,
        0x8000000080008081,
        0x8000000000008080,
        0x0000000080000001,
        0x8000000080008008,
    ]
    _R = [
        [0, 36, 3, 41, 18],
        [1, 44, 10, 45, 2],
        [62, 6, 43, 15, 61],
        [28, 55, 25, 21, 56],
        [27, 20, 39, 8, 14],
    ]

    def _rot(value: int, shift: int) -> int:
        return ((value << shift) & _MASK) | (value >> (64 - shift))

    def _keccak_f(state: List[int]) -> None:
        for rc in _RC:
            c = [state[x] ^ state[x + 5] ^ state[x + 10] ^ state[x + 15] ^ state[x + 20] for x in range(5)]
            d = [c[(x - 1) % 5] ^ _rot(c[(x + 1) % 5], 1) for x in range(5)]
            for x in range(5):
                for y in range(5):
                    state[x + 5 * y] ^= d[x]
            b = [0] * 25
            for x in range(5):
                for y in range(5):
                    nx = y
                    ny = (2 * x + 3 * y) % 5
                    b[nx + 5 * ny] = _rot(state[x + 5 * y], _R[x][y])
            for x in range(5):
                for y in range(5):
                    state[x + 5 * y] = b[x + 5 * y] ^ ((~b[((x + 1) % 5) + 5 * y] & _MASK) & b[((x + 2) % 5) + 5 * y])
            state[0] ^= rc

    rate = 136  # 1088 bits
    state = [0] * 25
    padded = bytearray(data)
    padded.append(0x01)
    while len(padded) % rate != 0:
        padded.append(0x00)
    padded[-1] ^= 0x80
    for offset in range(0, len(padded), rate):
        block = padded[offset : offset + rate]
        for i in range(rate // 8):
            state[i] ^= int.from_bytes(block[i * 8 : (i + 1) * 8], "little")
        _keccak_f(state)
    out = bytearray()
    while len(out) < 32:
        for lane in state[: rate // 8]:
            out.extend(lane.to_bytes(8, "little"))
        if len(out) >= 32:
            break
        _keccak_f(state)
    return bytes(out[:32])
