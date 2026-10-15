#!/usr/bin/env python3
"""Fail before building if shared Rust/TS Compose configuration has drifted."""

from __future__ import annotations

import json
import sys

SHARED_PROFILE_KEYS = {
    "MAX_INLINE_FILE_SIZE_BYTES",
    "UPLOAD_MAX_FILE_SIZE_BYTES",
    "UPLOAD_ALLOWED_EXTENSIONS",
    "DOWNLOAD_MAX_FILE_SIZE_BYTES",
    "REQUEST_BODY_LIMIT",
    "MAX_BUILD_CONCURRENCY",
    "TRAVERSE_EXCLUDE_DIRS",
    "BACKUP_TRAVERSE_EXCLUDE_FILES",
    "CONTENT_TRAVERSE_EXCLUDE_FILES",
    "INLINE_IMAGE_EXTENSIONS",
    "GIT_AUTO_GITIGNORE",
    "GIT_GITIGNORE_ENTRIES",
    "ZIP_WORKSPACE_EXCLUDE",
    "npm_config_store_dir",
    "XDG_CACHE_HOME",
    "HOME",
    "NPM_CONFIG_USERCONFIG",
}


def volume_target(volume: object) -> str | None:
    if isinstance(volume, dict):
        target = volume.get("target")
        return target if isinstance(target, str) else None
    if isinstance(volume, str):
        return volume.rsplit(":", 1)[-1]
    return None


def volume_source(volume: object) -> str | None:
    if isinstance(volume, dict):
        source = volume.get("source")
        return source if isinstance(source, str) else None
    if isinstance(volume, str):
        parts = volume.split(":")
        return parts[-2] if len(parts) >= 2 else None
    return None


def mounted_volume_source(service: object, target: str) -> str | None:
    if not isinstance(service, dict):
        return None
    for volume in service.get("volumes", []):
        if volume_target(volume) == target:
            return volume_source(volume)
    return None


def main() -> None:
    config = json.load(sys.stdin)
    services = config.get("services", {})
    if any("build" in service for service in services.values()):
        raise SystemExit("Runtime Compose must contain images only, never build definitions")
    rust = services.get("rust", {}).get("environment", {})
    typescript = services.get("typescript", {}).get("environment", {})
    missing = {
        service: sorted(SHARED_PROFILE_KEYS - environment.keys())
        for service, environment in (("rust", rust), ("typescript", typescript))
        if SHARED_PROFILE_KEYS - environment.keys()
    }
    if missing:
        print(
            "A/B shared Compose profile is missing required environment variables: "
            f"{json.dumps(missing, sort_keys=True)}",
            file=sys.stderr,
        )
        raise SystemExit(1)
    shared_keys = rust.keys() & typescript.keys()
    mismatches = {
        key: {"rust": rust[key], "typescript": typescript[key]}
        for key in sorted(shared_keys)
        if rust[key] != typescript[key]
    }
    if mismatches:
        print(
            "Rust/TypeScript A/B services have different values for shared "
            f"environment variables: {json.dumps(mismatches, sort_keys=True)}",
            file=sys.stderr,
        )
        raise SystemExit(1)
    missing_pnpm_store_mount = {
        service: ["/pnpm-cache"]
        for service in ("rust", "typescript")
        if not any(
            volume_target(volume) == "/pnpm-cache"
            for volume in services.get(service, {}).get("volumes", [])
        )
    }
    if missing_pnpm_store_mount:
        print(
            "A/B services must mount the persistent pnpm store at /pnpm-cache: "
            f"{json.dumps(missing_pnpm_store_mount, sort_keys=True)}",
            file=sys.stderr,
        )
        raise SystemExit(1)
    wrong_pnpm_store_dir = {
        service: environment.get("npm_config_store_dir")
        for service, environment in (("rust", rust), ("typescript", typescript))
        if environment.get("npm_config_store_dir") != "/pnpm-cache"
    }
    if wrong_pnpm_store_dir:
        print(
            "A/B services must configure pnpm's npm_config_store_dir as /pnpm-cache: "
            f"{json.dumps(wrong_pnpm_store_dir, sort_keys=True)}",
            file=sys.stderr,
        )
        raise SystemExit(1)
    missing_pnpm_metadata_mount = {
        service: ["/pnpm-metadata"]
        for service in ("rust", "typescript")
        if not any(
            volume_target(volume) == "/pnpm-metadata"
            for volume in services.get(service, {}).get("volumes", [])
        )
    }
    if missing_pnpm_metadata_mount:
        print(
            "A/B services must mount the persistent pnpm metadata cache at /pnpm-metadata: "
            f"{json.dumps(missing_pnpm_metadata_mount, sort_keys=True)}",
            file=sys.stderr,
        )
        raise SystemExit(1)
    wrong_pnpm_metadata_dir = {
        service: environment.get("XDG_CACHE_HOME")
        for service, environment in (("rust", rust), ("typescript", typescript))
        if environment.get("XDG_CACHE_HOME") != "/pnpm-metadata"
    }
    if wrong_pnpm_metadata_dir:
        print(
            "A/B services must point XDG_CACHE_HOME at the persistent pnpm metadata cache: "
            f"{json.dumps(wrong_pnpm_metadata_dir, sort_keys=True)}",
            file=sys.stderr,
        )
        raise SystemExit(1)

    wrong_pnpm_user_config = {
        service: environment.get("NPM_CONFIG_USERCONFIG")
        for service, environment in (("rust", rust), ("typescript", typescript))
        if environment.get("NPM_CONFIG_USERCONFIG") != "/etc/npmrc"
    }
    if wrong_pnpm_user_config:
        print(
            "A/B services must read the shared pnpm runtime configuration from /etc/npmrc: "
            f"{json.dumps(wrong_pnpm_user_config, sort_keys=True)}",
            file=sys.stderr,
        )
        raise SystemExit(1)

    pnpm_store_sources = {
        service: mounted_volume_source(services.get(service), "/pnpm-cache")
        for service in ("rust", "typescript")
    }
    if (
        not pnpm_store_sources["rust"]
        or not pnpm_store_sources["typescript"]
        or pnpm_store_sources["rust"] == pnpm_store_sources["typescript"]
    ):
        print(
            "Rust and TypeScript must use separate persistent pnpm package stores for independent cold installs: "
            f"{json.dumps(pnpm_store_sources, sort_keys=True)}",
            file=sys.stderr,
        )
        raise SystemExit(1)
    pnpm_metadata_sources = {
        service: mounted_volume_source(services.get(service), "/pnpm-metadata")
        for service in ("rust", "typescript")
    }
    if (
        not pnpm_metadata_sources["rust"]
        or not pnpm_metadata_sources["typescript"]
        or pnpm_metadata_sources["rust"] == pnpm_metadata_sources["typescript"]
    ):
        print(
            "Rust and TypeScript must use separate persistent pnpm metadata caches: "
            f"{json.dumps(pnpm_metadata_sources, sort_keys=True)}",
            file=sys.stderr,
        )
        raise SystemExit(1)
    if mounted_volume_source(services.get("toolchain"), "/pnpm-cache"):
        print("toolchain image must not mount or pre-populate template pnpm stores", file=sys.stderr)
        raise SystemExit(1)

    workspace_sources = {
        "rust": mounted_volume_source(services.get("rust"), "/data/project-workspace"),
        "typescript": mounted_volume_source(services.get("typescript"), "/data/project-workspace"),
    }
    if (
        not workspace_sources["rust"]
        or not workspace_sources["typescript"]
        or workspace_sources["rust"] == workspace_sources["typescript"]
    ):
        print(
            "Rust and TypeScript project workspaces must use separate Docker-managed volumes: "
            f"{json.dumps(workspace_sources, sort_keys=True)}",
            file=sys.stderr,
        )
        raise SystemExit(1)
    missing_workspace_volume_links = {
        service_name: {
            side: {
                "expected": workspace_sources[side],
                "actual": mounted_volume_source(
                    services.get(service_name),
                    f"/ab-runtime/{side}/project-workspace"
                    if service_name == "driver"
                    else f"/workspace/{side}",
                ),
            }
            for side in ("rust", "typescript")
            if mounted_volume_source(
                services.get(service_name),
                f"/ab-runtime/{side}/project-workspace"
                if service_name == "driver"
                else f"/workspace/{side}",
            )
            != workspace_sources[side]
        }
        for service_name in ("driver",)
        if any(
            mounted_volume_source(
                services.get(service_name),
                f"/ab-runtime/{side}/project-workspace"
                if service_name == "driver"
                else f"/workspace/{side}",
            )
            != workspace_sources[side]
            for side in ("rust", "typescript")
        )
    }
    if missing_workspace_volume_links:
        print(
            "A/B services and driver must share each isolated project-workspace volume: "
            f"{json.dumps(missing_workspace_volume_links, sort_keys=True)}",
            file=sys.stderr,
        )
        raise SystemExit(1)
    print(f"A/B shared Compose environment matches ({len(shared_keys)} variables)")


if __name__ == "__main__":
    main()
