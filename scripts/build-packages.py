#!/usr/bin/env python3
import argparse
import json
import subprocess
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True)
    parser.add_argument("--platform", default="linux")
    parser.add_argument("--arch", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    target_dir = root / "target" / args.target / "release"
    for plugin in json.loads((root / "plugins.json").read_text()):
        output = args.output / f"{plugin['id']}-{plugin['version']}-{args.platform}-{args.arch}.zip"
        subprocess.run([
            "python3", str(root / "scripts/package_plugin.py"),
            "--id", plugin["id"],
            "--version", plugin["version"],
            "--manifest", str(root / plugin["manifest"]),
            "--binary", str(target_dir / plugin["binary"]),
            "--platform", args.platform,
            "--arch", args.arch,
            "--output", str(output),
        ], check=True)


if __name__ == "__main__":
    main()
