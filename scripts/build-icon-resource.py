"""Rebuild the checked-in x64 icon resource; normal Cargo builds need no tools."""

import argparse
import os
from pathlib import Path
import struct
import subprocess


def resource(kind, name, data, flags=0x1030):
    header = struct.pack(
        "<IIHHHHIHHII", len(data), 32, 0xFFFF, kind, 0xFFFF, name,
        0, flags, 0, 0, 0,
    )
    return header + data + b"\0" * (-len(data) % 4)


def icon_resource(ico):
    if len(ico) < 6:
        raise ValueError("Truncated ICO header")
    reserved, kind, count = struct.unpack_from("<HHH", ico)
    if reserved or kind != 1 or not count or len(ico) < 6 + count * 16:
        raise ValueError("Invalid ICO directory")
    records = [resource(0, 0, b"", flags=0)]
    group = bytearray(struct.pack("<HHH", 0, 1, count))
    for index in range(count):
        width, height, colors, unused, planes, bits, size, offset = struct.unpack_from(
            "<BBBBHHII", ico, 6 + index * 16
        )
        if unused or offset < 6 + count * 16 or size == 0 or offset + size > len(ico):
            raise ValueError("Invalid ICO image bounds")
        identifier = index + 1
        records.append(resource(3, identifier, ico[offset:offset + size]))
        group.extend(struct.pack(
            "<BBBBHHIH", width, height, colors, 0, planes, bits, size, identifier
        ))
    records.append(resource(14, 1, group))
    return b"".join(records)


def remove_build_metadata(data):
    """Clear CVTRES CodeView paths without changing resource/relocation offsets."""
    obj = bytearray(data)
    if len(obj) < 20:
        raise ValueError("Truncated COFF header")
    machine, count, _, symbols, symbol_count, optional_size, _ = struct.unpack_from(
        "<HHIIIHH", obj
    )
    if machine != 0x8664 or optional_size or len(obj) < 20 + count * 40:
        raise ValueError("Expected an x64 COFF resource object")
    if symbols + symbol_count * 18 > len(obj):
        raise ValueError("Invalid COFF symbol table")
    # CVTRES records absolute working-directory and executable paths even when
    # its input/output arguments are relative. Empty the nonfunctional debug
    # section in place; retain all offsets used by the resource relocations.
    debug_sections = set()
    for index in range(count):
        header = 20 + index * 40
        if obj[header:header + 8].rstrip(b"\0") != b".debug$S":
            continue
        size, offset, _, _, relocations, line_numbers = struct.unpack_from(
            "<IIIIHH", obj, header + 16
        )
        if (relocations or line_numbers or offset + size > len(obj)
                or (size and offset < 20 + count * 40)):
            raise ValueError("Unsupported COFF debug section")
        obj[offset:offset + size] = b"\0" * size
        struct.pack_into("<II", obj, header + 16, 0, 0)
        debug_sections.add(index + 1)
    index = 0
    while index < symbol_count:
        position = symbols + index * 18
        _, _, section, _, storage, auxiliary = struct.unpack_from(
            "<8sIhHBB", obj, position
        )
        if index + auxiliary >= symbol_count:
            raise ValueError("Invalid COFF auxiliary symbol")
        if section in debug_sections and storage == 3 and auxiliary == 1:
            struct.pack_into("<I", obj, position + 18, 0)
        index += 1 + auxiliary
    # Avoid recording local build time in the checked-in resource object.
    struct.pack_into("<I", obj, 4, 0)
    return bytes(obj)


def main():
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cvtres", type=Path)
    args = parser.parse_args()
    converter = args.cvtres or (
        Path(os.environ.get("WINDIR", r"C:\Windows"))
        / "Microsoft.NET/Framework64/v4.0.30319/cvtres.exe"
    )
    if not converter.is_file():
        parser.error("CVTRES not found; provide --cvtres with its absolute path")
    output = root / "outputs"
    output.mkdir(exist_ok=True)
    res = output / "featherdock.res"
    res.write_bytes(icon_resource((root / "assets/featherdock.ico").read_bytes()))
    target = root / "assets/featherdock.res.o"
    intermediate = output / "featherdock.res.o"
    subprocess.run([
        str(converter), "/NOLOGO", "/MACHINE:X64", "/READONLY",
        f"/OUT:{intermediate.relative_to(root)}", str(res.relative_to(root)),
    ], cwd=root, check=True)
    target.write_bytes(remove_build_metadata(intermediate.read_bytes()))
    print(f"Updated {target}")


if __name__ == "__main__":
    main()
