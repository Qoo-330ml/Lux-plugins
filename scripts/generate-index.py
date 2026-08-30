#!/usr/bin/env python3
import argparse
import hashlib
import json
import re
from pathlib import Path
from zipfile import ZipFile


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    grouped = {}
    for package in sorted(args.artifacts.rglob("*.zip")):
        with ZipFile(package) as archive:
            manifest = json.loads(archive.read("manifest.json"))
        match = re.search(r"-linux-(x86_64|aarch64)\.zip$", package.name)
        if not match:
            raise SystemExit(f"package name does not contain a supported architecture: {package.name}")
        arch = match.group(1)
        entry = grouped.setdefault(manifest["id"], {
            "id": manifest["id"],
            "name": manifest["name"],
            "description": manifest.get("description", ""),
            "category": manifest["category"],
            "version": manifest["version"],
            "runtime": manifest["runtime"]["kind"],
            "providerKey": manifest.get("providerKey"),
            "aliases": manifest.get("aliases", []),
            "capabilities": manifest.get("capabilities", []),
            "packages": [],
        })
        if entry["version"] != manifest["version"]:
            raise SystemExit(f"version mismatch for {manifest['id']}")
        digest = hashlib.sha256(package.read_bytes()).hexdigest()
        asset = package.name
        entry["packages"].append({
            "platform": "linux",
            "arch": arch,
            "url": f"https://github.com/{args.repository}/releases/download/{manifest['id']}/{asset}",
            "sha256": digest,
        })

    result = {
        "formatVersion": 1,
        "name": "Lux Plugins",
        "description": "Lux 官方插件目录",
        "plugins": sorted(grouped.values(), key=lambda item: item["id"]),
    }
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
