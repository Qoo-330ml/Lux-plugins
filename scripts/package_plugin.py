#!/usr/bin/env python3
import argparse
import hashlib
import json
from pathlib import Path
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--id", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--arch", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    binary = args.binary.read_bytes()
    if not binary:
        raise SystemExit("plugin binary is empty")
    binary_name = args.binary.name + (".exe" if args.platform == "windows" and not args.binary.name.endswith(".exe") else "")
    relative_binary = f"binaries/{args.platform}-{args.arch}/{binary_name}"
    manifest = json.loads(args.manifest.read_text())
    manifest["version"] = args.version
    manifest["runtime"]["entrypoint"] = relative_binary
    manifest["files"] = [{"path": relative_binary, "sha256": hashlib.sha256(binary).hexdigest()}]
    args.output.parent.mkdir(parents=True, exist_ok=True)

    with ZipFile(args.output, "w", compression=ZIP_DEFLATED) as archive:
        archive.writestr("manifest.json", json.dumps(manifest, ensure_ascii=False, indent=2) + "\n")
        entry = ZipInfo(relative_binary)
        entry.compress_type = ZIP_DEFLATED
        entry.external_attr = 0o755 << 16
        archive.writestr(entry, binary)


if __name__ == "__main__":
    main()
