#!/usr/bin/env python3
"""Create deterministic, offline fixtures for the file-server A/B driver."""

from __future__ import annotations

import base64
import io
import json
import sys
import zipfile
from pathlib import Path


FIXED_TIME = (2024, 1, 2, 3, 4, 6)


def archive(path: Path, files: dict[str, bytes]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as out:
        for name, body in sorted(files.items()):
            entry = zipfile.ZipInfo(name, FIXED_TIME)
            entry.compress_type = zipfile.ZIP_DEFLATED
            entry.external_attr = 0o100644 << 16
            out.writestr(entry, body)


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: prepare-fixtures.py OUTPUT_DIR")
    root = Path(sys.argv[1])
    root.mkdir(parents=True, exist_ok=True)

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
