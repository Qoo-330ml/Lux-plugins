#!/usr/bin/env python3
import argparse
import json
import subprocess
from pathlib import Path

from release_plan import select_plugins


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True)
    parser.add_argument("--platform", default="linux")
    parser.add_argument("--arch", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--plugin-ids-json",
        help="JSON array of plugin IDs whose catalog version was added or changed",
    )
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    target_dir = root / "target" / args.target / "release"
    plugins = json.loads((root / "plugins.json").read_text())
    try:
        plugin_ids = json.loads(args.plugin_ids_json) if args.plugin_ids_json is not None else None
        plugins = select_plugins(plugins, plugin_ids)
    except (json.JSONDecodeError, ValueError) as error:
        raise SystemExit(f"invalid plugin release selection: {error}") from error
    if not plugins:
        raise SystemExit("no plugin packages selected for this release")
    for plugin in plugins:
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
