#!/usr/bin/env python3
"""Create deterministic, offline fixtures for the file-server A/B driver."""

from __future__ import annotations

import base64
import hashlib
import io
import json
import sys
import zipfile
from pathlib import Path


FIXED_TIME = (2024, 1, 2, 3, 4, 6)
PNPM_LOCK_FIXTURES = {
    "react-vite-template.zip": {
        "package_json_sha256": "5640755b2e01243d8462d5294804a256fb4b477b4dddba0edbfd7b10c8d0c505",
        "lock_file": "react-vite-template.pnpm-lock.yaml",
    },
    "vue3-vite-template.zip": {
        "package_json_sha256": "6460fb7707f31db7d93729de4ca0bc0a6393dea515144676b25328891644f2e8",
        "lock_file": "vue3-vite-template.pnpm-lock.yaml",
    },
}


def archive(path: Path, files: dict[str, bytes]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as out:
        for name, body in sorted(files.items()):
            entry = zipfile.ZipInfo(name, FIXED_TIME)
            entry.compress_type = zipfile.ZIP_DEFLATED
            entry.external_attr = 0o100644 << 16
            out.writestr(entry, body)


def add_pnpm_profile(
    path: Path, registry: str, network_concurrency: str, lock_mode: str
) -> None:
    with zipfile.ZipFile(path, "r") as source:
        entries = [(info, source.read(info.filename)) for info in source.infolist()]
    if any(info.filename == ".npmrc" for info, _ in entries):
        raise SystemExit(f"template fixture already contains .npmrc; review before overriding: {path}")
    package_json = next(
        (data for info, data in entries if info.filename == "package.json"), None
    )
    if package_json is None:
        raise SystemExit(f"template fixture has no root package.json: {path}")

    source_lock = next(
        (data for info, data in entries if info.filename == "pnpm-lock.yaml"), None
    )
    if source_lock is None and lock_mode == "pinned":
        lock_fixture = PNPM_LOCK_FIXTURES.get(path.name)
        if lock_fixture is None:
            raise SystemExit(f"template fixture has no pinned pnpm lockfile: {path}")
        package_hash = hashlib.sha256(package_json).hexdigest()
        if package_hash != lock_fixture["package_json_sha256"]:
            raise SystemExit(
                f"{path.name} package.json changed (sha256={package_hash}); "
                "regenerate and review its pinned A/B pnpm-lock.yaml"
            )
        lock_path = (
            Path(__file__).resolve().parents[2]
            / "crates"
            / "file-server-ab"
            / "fixtures"
            / "pnpm-locks"
            / lock_fixture["lock_file"]
        )
        if not lock_path.is_file():
            raise SystemExit(f"pinned pnpm lockfile fixture is missing: {lock_path}")
        lock_data = lock_path.read_bytes()
    else:
        # Preserve a lockfile already supplied by the original template ZIP.
        lock_data = None

    # Keep PNPM's default package import method. Forcing "copy" makes the
    # cache-to-node_modules linking step copy every package file even when the
    # filesystem can clone or hard-link it.
    npmrc = (
        "auto-install-peers=true\n"
        f"registry={registry}\n"
        "store-dir=/pnpm-cache\n"
        f"network-concurrency={network_concurrency}\n"
    ).encode()
    temp_path = path.with_suffix(path.suffix + ".tmp")
    with zipfile.ZipFile(temp_path, "w") as output:
        for info, data in entries:
            output.writestr(info, data)
        if lock_data is not None:
            lock_info = zipfile.ZipInfo("pnpm-lock.yaml", FIXED_TIME)
            lock_info.compress_type = zipfile.ZIP_DEFLATED
            lock_info.external_attr = 0o100644 << 16
            output.writestr(lock_info, lock_data)
        npmrc_info = zipfile.ZipInfo(".npmrc", FIXED_TIME)
        npmrc_info.compress_type = zipfile.ZIP_DEFLATED
        npmrc_info.external_attr = 0o100644 << 16
        output.writestr(npmrc_info, npmrc)
    temp_path.replace(path)


def main() -> None:
    if len(sys.argv) != 5:
        raise SystemExit(
            "usage: prepare-fixtures.py OUTPUT_DIR PNPM_REGISTRY "
            "PNPM_NETWORK_CONCURRENCY PNPM_LOCK_MODE"
        )
    root = Path(sys.argv[1])
    registry = sys.argv[2]
    network_concurrency = sys.argv[3]
    lock_mode = sys.argv[4]
    if not network_concurrency.isdecimal() or int(network_concurrency) < 1:
        raise SystemExit("PNPM_NETWORK_CONCURRENCY must be a positive integer")
    if lock_mode not in {"pinned", "source"}:
        raise SystemExit("PNPM_LOCK_MODE must be 'pinned' or 'source'")
    root.mkdir(parents=True, exist_ok=True)
    for template_name in ("react-vite-template.zip", "vue3-vite-template.zip"):
        fixture_zip = root / template_name
        add_pnpm_profile(fixture_zip, registry, network_concurrency, lock_mode)

    archive(
        root / "skills-fixture.zip",
        {
            "skills/file-server-ab-skill/SKILL.md": b"# A-B fixture skill\n\nDeterministic offline fixture.\n",
            "skills/file-server-ab-skill/references/guide.txt": b"local skill reference\n",
        },
    )
    archive(
        root / "workspace-project.zip",
        {
            "project-wrapper/README.md": b"A/B imported project\n",
            "project-wrapper/src/imported.txt": b"imported from local fixture\n",
            "project-wrapper/.agents/from-archive.txt": b"must not replace agent state\n",
        },
    )

    artifact = io.BytesIO()
    with zipfile.ZipFile(artifact, "w", compression=zipfile.ZIP_DEFLATED) as out:
        entry = zipfile.ZipInfo("payload.txt", FIXED_TIME)
        entry.compress_type = zipfile.ZIP_DEFLATED
        entry.external_attr = 0o100644 << 16
        out.writestr(entry, b"synthetic package artifact\n")
    artifact_b64 = base64.b64encode(artifact.getvalue()).decode("ascii")
    package_json = json.dumps(
        {
            "name": "file-server-ab-package-fixture",
            "version": "1.0.0",
            "private": True,
            "type": "module",
        },
        sort_keys=True,
        separators=(",", ":"),
    ) + "\n"
    script = f'''import fs from "node:fs/promises";
import path from "node:path";

const [agentName, version, outputDirectory] = process.argv.slice(2);
if (!agentName || !version || !outputDirectory) {{
  throw new Error("expected agent name, version, and output directory");
}}
const artifactName = `${{agentName}}-linux-x64-${{version}}.zip`;
const outputPath = path.join(outputDirectory, artifactName);
await fs.mkdir(outputDirectory, {{ recursive: true }});
await fs.writeFile(outputPath, Buffer.from("{artifact_b64}", "base64"));
console.log(path.relative(process.cwd(), outputPath).split(path.sep).join("/"));
'''
    archive(
        root / "package-project.zip",
        {
            "agent-package/package.json": package_json.encode(),
            "agent-package/README.md": b"Offline package fixture\n",
            "agent-package/scripts/package-platforms.mjs": script.encode(),
        },
    )


if __name__ == "__main__":
    main()
