#!/usr/bin/env python3
"""One-shot generator for rpcrypt_tables.rs from the C reference rpcrypt.c."""
import re
import sys

SRC = r"F:\projekte\chiaki-rust-remaster\lib\src\rpcrypt.c"
OUT = r"F:\projekte\chiaki-rs\crates\chiaki-core\src\rpcrypt_tables.rs"

text = open(SRC, "r", encoding="utf-8").read()

pattern = re.compile(
    r"static\s+const\s+uint8_t\s+(\w+)\s*\[[^\]]*\]\s*=\s*\{([^}]*)\}", re.S
)

seen = {}
order = []
for m in pattern.finditer(text):
    name = m.group(1)
    body = m.group(2)
    vals = [int(x, 16) for x in re.findall(r"0x[0-9a-fA-F]+", body)]
    data = bytes(vals)
    if name in seen:
        if seen[name] != data:
            sys.exit(f"conflicting duplicate for {name}")
        continue
    seen[name] = data
    order.append(name)

print("extracted arrays:")
for n in order:
    print(f"  {n}: {len(seen[n])} bytes")

# sanity checks against expectations
expect = {
    "keys_a_ps4": 0x70 * 0x20,
    "keys_a_ps5": 0x70 * 0x20,
    "keys_b_ps4": 0x70 * 0x20,
    "keys_b_ps5": 0x70 * 0x20,
    "ps4_keys_1": 512,
    "ps5_keys_1": 512,
    "ps4_keys_0": 512,
    "ps5_keys_0": 512,
    "echo_a": 16,
    "echo_b": 16,
    "regist_aes_key": 16,
    "hmac_key_ps5": 16,
    "hmac_key_ps4": 16,
    "hmac_key_ps4_pre10": 16,
}
missing = set(expect) - set(seen)
if missing:
    sys.exit(f"missing arrays: {missing}")
for n, sz in expect.items():
    if len(seen[n]) != sz:
        sys.exit(f"size mismatch for {n}: {len(seen[n])} != {sz}")

lines = []
lines.append("// AUTO-GENERATED from F:\\projekte\\chiaki-rust-remaster\\lib\\src\\rpcrypt.c")
lines.append("// by gen_rpcrypt_tables.py - DO NOT EDIT BY HAND.")
lines.append("// Byte-exact static sigil/key tables used by the RPCrypt (chiaki-ng).")
lines.append("#![allow(clippy::all)]")
lines.append("")
for n in order:
    data = seen[n]
    const_name = n.upper()
    lines.append(f"pub const {const_name}: [u8; {len(data)}] = [")
    for i in range(0, len(data), 16):
        chunk = data[i : i + 16]
        lines.append("    " + ", ".join(f"0x{b:02x}" for b in chunk) + ",")
    lines.append("];")
    lines.append("")

open(OUT, "w", encoding="utf-8", newline="\n").write("\n".join(lines))
print(f"wrote {OUT} ({len(lines)} lines)")
