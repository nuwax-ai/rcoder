# RCoder CLI npm Packages

This directory contains npm package templates for distributing rcoder-cli binaries.

## Package Structure

```
npm/
├── rcoder-cli/                    # Main package (platform detection + wrapper)
│   ├── package.json
│   └── bin/cli.js
├── rcoder-cli-linux-x64/          # Linux x86_64 binary
├── rcoder-cli-linux-arm64/        # Linux ARM64 binary
├── rcoder-cli-darwin-x64/         # macOS x86_64 binary
├── rcoder-cli-darwin-arm64/       # macOS ARM64 binary
└── rcoder-cli-win32-x64/          # Windows x86_64 binary
```

## Installation

### Stable version
```bash
npm install -g rcoder-cli
```

### Beta version
```bash
npm install -g rcoder-cli@beta
```

### Using China mirror
```bash
npm config set registry https://registry.npmmirror.com
npm install -g rcoder-cli
```

## CI/CD

Two GitHub Actions workflows handle publishing:

- `release.yml` - Publishes stable versions with `@latest` tag
  - Triggered by tags like `v1.0.0` or `1.0.0`
- `release-beta.yml` - Publishes beta versions with `@beta` tag
  - Triggered by tags like `v1.0.0-beta.1` or `1.0.0-beta.1`

## How it works

1. User runs `npm install -g rcoder-cli`
2. npm installs main package + optional dependency for current platform
3. When user runs `rcoder-cli`, `bin/cli.js` detects platform and executes the binary from the platform package

## Version Management

All packages share the same version number. The CI automatically updates versions from the git tag before publishing.
