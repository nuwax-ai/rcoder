# RCoder - AI-Powered Development Platform

> [中文](README.md)

RCoder is an AI development platform built with Rust and ACP (Agent Client Protocol). It exposes Claude Code, Codex and other agents through HTTP/SSE, and manages UserApp applications from source development and builds to deployment. The same codebase supports Docker and Kubernetes runtimes.

## Features

- **Agent sessions**: unified ACP access, internal gRPC communication, live SSE progress and reconnection support.
- **UserApp hosting**: development and production environments, builds and releases, idle recycling, traffic-based wake-up, and stop/recovery workflows that preserve data.
- **Files and development services**: workspaces, Git, skills, development servers and application logs.
- **Computer Agent**: containerized desktops with VNC, audio and IME passthrough.
- **Dual runtimes**: local Docker and Kubernetes orchestration with shared business contracts.
- **Separate storage choices**: agent projects/sessions use memory or PostgreSQL; UserApp control records use local Turso or PostgreSQL on Kubernetes.
- **APIs and observability**: OpenAPI, Tracing, OpenTelemetry and optional performance diagnostics.

## Architecture

```text
Client HTTP/SSE
    ↓
RCoder: HTTP interface + engine orchestration + container runtime
    ↓ Tonic gRPC
agent_runner: in-container execution environment
    ↓ ACP
AI agent
```

HTTP handlers live in `http-server`, orchestration in `rcoder-engine`, and application assembly in `rcoder`. UserApp uses the application management layer, Docker/Kubernetes runtime and in-container app-cli. See the [architecture overview](docs/architecture/overview.md).

<a id="quick-start"></a>

## Quick Start

Local Docker Compose is the recommended starting point. Install Docker, Compose and Make. Source builds use Rust stable with Edition 2024; see the [contributing guide](CONTRIBUTING.md#开发环境) for prerequisites.

```bash
git clone https://github.com/nuwax-ai/rcoder.git
cd rcoder
make dev-build
make dev-up
curl http://127.0.0.1:8090/health
```

Open the [API documentation](http://127.0.0.1:8090/api/docs) after startup. The health check verifies the control plane. Real AI sessions also require an agent, a model endpoint and credentials; see the [configuration guide](docs/configuration.md).

After Rust changes, use `make dev-hot` to rebuild incrementally and restart. Use `make dev-logs` for logs and `make dev-down` to stop. The [Docker guide](docs/deployment/docker.md) covers images, persistence and the full workflow.

## Documentation

The complete index is [docs/README.md](docs/README.md). Detailed guides are currently maintained in Chinese.

| Goal | Guide |
|---|---|
| Configuration and model setup | [Configuration](docs/configuration.md) |
| Deployment | [Docker Compose](docs/deployment/docker.md), [host machine](docs/deployment/host.md), [Kubernetes](docs/deployment/kubernetes.md) |
| Session integration | [Agents and SSE](docs/concepts/agent-sessions.md), [permissions](docs/concepts/permissions.md) |
| Applications and files | [UserApp](docs/concepts/userapp.md), [file services](docs/concepts/file-services.md), [Computer Agent](docs/concepts/computer-agent.md) |
| Builds and service management | [app-cli](docs/development/app-cli.md), [configuration validation](docs/development/app-cli-validate.md) |
| Operations and troubleshooting | [Errors and retries](docs/runtime-error-contract.md), [owner recovery](docs/userapp-dev-owner-recovery.md), [observability](docs/observability.md) |

The runtime-generated OpenAPI is the full API contract: `/api/docs` provides Swagger UI and `/api/docs/scalar` provides Scalar, including main-service and file-server documents.

## Development and Verification

See [CONTRIBUTING.md](CONTRIBUTING.md) for environment setup, builds, formatting, Clippy and tests. Use `make test` for the root workspace. `crates/app-cli` is a separate Cargo project and requires its own checks.

Run `make test-e2e` for real Compose business regressions. Prerequisites and acceptance boundaries are documented in the [E2E guide](tests-e2e/tools/README.md). Record component tests, deployment acceptance and releases separately.

## Contributing and Security

Contributions through [Issues](https://github.com/nuwax-ai/rcoder/issues) and pull requests are welcome. Read the [contributing guide](CONTRIBUTING.md) first; report vulnerabilities privately through [SECURITY.md](SECURITY.md).

## License

Licensed under [Apache-2.0](LICENSE).
