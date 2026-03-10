# RCoder - AI-Powered Development Platform

RCoder is a modern AI-powered development platform built with Rust. It provides unified interaction with multiple AI agents through the **SACP (Symposium ACP)** protocol, featuring a **microservice architecture** with **Docker containerized deployment** and **high-performance gRPC communication**.

> [中文文档](README.md)

## Features

- **Reverse Proxy** - Cloudflare Pingora integration with high-performance port routing `/proxy/{port}/{path}`
- **HTTP API** - Modern REST API built on Axum with unified SSE progress streaming
- **Multi-Agent Support** - Unified access to Claude Code, Codex, and other AI agents
- **Containerized Architecture** - Per-project Docker containers for isolation and resource management
- **gRPC Communication** - High-performance internal communication via Tonic with Server Streaming
- **Configuration System** - Multi-layered priority: CLI args > Environment variables > Config file
- **API Documentation** - Auto-generated API docs (utoipa + Swagger UI)
- **Observability** - Tracing + OpenTelemetry distributed tracing + Pyroscope profiling
- **Computer Agent** - Containerized AI agent environment with VNC remote desktop, audio streaming, and IME input

## Architecture

### Overview

```
External Client (HTTP/SSE)
    |
RCoder (HTTP API Server + Docker Management + gRPC Client)
    | gRPC (Chat, CancelSession, SubscribeProgress)
Agent Runner (gRPC Server in Docker)
    | Server Streaming (real-time progress events)
RCoder (converts to SSE)
    |
External Client (SSE)
```

### Core Components

- **RCoder Main Service** - Axum HTTP server + container management + gRPC client
- **Agent Runner** - Isolated AI agent runtime environment (inside Docker), provides gRPC service
- **Pingora Proxy** - High-performance reverse proxy with port-based routing
- **Docker Manager** - Global container lifecycle management

### Tech Stack

| Category | Technology | Description |
|----------|-----------|-------------|
| **Language** | Rust 2024 Edition | Modern systems programming language |
| **HTTP Framework** | Axum + Tower | High-performance async web framework |
| **RPC Framework** | Tonic (gRPC) | High-performance RPC communication |
| **AI Protocol** | SACP + MCP | Multi-agent protocol support |
| **Containerization** | Docker + Bollard | Container management and orchestration |
| **Database** | DuckDB + SQLx | Embedded analytical database |
| **Logging** | Tracing + OpenTelemetry | Structured logging and distributed tracing |
| **Profiling** | Pyroscope | Continuous performance profiling |
| **CLI** | clap | Modern command-line argument parsing |

## Getting Started

### Prerequisites

- Rust 1.75+ (2024 Edition)
- Docker (for containerized deployment)
- Optional: Claude Code CLI (for Claude agent)

### Installation & Running

#### Local Development

```bash
# Clone the repository
git clone https://github.com/your-org/rcoder.git
cd rcoder

# Build all crates
cargo build --workspace

# Run the main service
cargo run --bin rcoder

# Specify port and projects directory
cargo run --bin rcoder -- --port 8087 --projects-dir ./my-projects
```

#### Docker Development Mode (Recommended)

```bash
# Build images and start containers
make dev-build    # Build Docker images
make dev-up       # Start development containers

# Restart after code changes
make dev-restart  # Rebuild and restart

# View logs
make dev-logs

# Stop containers
make dev-down
```

#### Enable Reverse Proxy

```bash
# Enable Pingora reverse proxy
cargo run --bin rcoder -- --enable-proxy --proxy-port 8080

# Specify default backend port
cargo run --bin rcoder -- --enable-proxy --proxy-port 8080 --backend-port 3000
```

### CLI Arguments

| Argument | Short | Description | Example |
|----------|-------|-------------|---------|
| `--port` | `-p` | Set main service port | `--port 8087` |
| `--projects-dir` | `-d` | Set project workspace directory | `--projects-dir ./projects` |
| `--enable-proxy` | - | Enable Pingora reverse proxy | `--enable-proxy` |
| `--proxy-port` | - | Set Pingora listen port | `--proxy-port 8080` |
| `--backend-port` | - | Default backend port | `--backend-port 3000` |

```bash
# View all arguments
cargo run --bin rcoder -- --help
```

## API Reference

### Pingora Reverse Proxy

Pingora is the built-in high-performance reverse proxy.

```bash
# Enable proxy
cargo run --bin rcoder -- --enable-proxy --proxy-port 8080

# Proxy request example (forward to port 5173)
curl "http://127.0.0.1:8080/proxy/5173/page/123/"
```

### Core Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Health check |
| `/chat` | POST | Send chat message to AI agent |
| `/agent/progress/{session_id}` | GET (SSE) | Real-time progress stream |
| `/agent/session/cancel` | POST | Cancel an active task |
| `/agent/stop` | POST | Stop the Agent |
| `/agent/status/{project_id}` | GET | Query Agent status |
| `/api/docs` | GET | Swagger UI API documentation |

### gRPC Services (Agent Runner)

| Method | Type | Description |
|--------|------|-------------|
| `Chat` | Unary | Send chat request |
| `SubscribeProgress` | Server Streaming | Subscribe to progress event stream |
| `CancelSession` | Unary | Cancel a session task |
| `GetStatus` | Unary | Query Agent status |
| `StopAgent` | Unary | Stop the Agent |
| `GetContainerStatus` | Unary | Query container status |
| `GetVncStatus` | Unary | Query VNC service status |

### Computer Agent Endpoints

Computer Agent provides a containerized AI agent environment with VNC remote desktop, audio streaming, and IME input support. Each user gets an isolated Docker container, and multiple projects can share the same container.

#### Core Interfaces

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/computer/chat` | POST | Send chat message to Computer Agent |
| `/computer/progress/{session_id}` | GET (SSE) | Real-time progress stream |
| `/computer/agent/stop` | POST | Stop Agent for a specific project (container stays alive) |
| `/computer/agent/status` | POST | Query Agent status (alive/idle/busy) |
| `/computer/agent/session/cancel` | POST | Cancel an active session |

#### Desktop & Media Proxy (via Pingora)

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/computer/desktop/{user_id}/{project_id}` | GET | Get VNC desktop access URLs |
| `/computer/vnc/{user_id}/{project_id}/{*path}` | GET | VNC/noVNC proxy (port 6080) |
| `/computer/audio/{user_id}/{project_id}/{*path}` | GET | Audio stream proxy (port 6089/6090) |
| `/computer/ime/{user_id}/{project_id}/{*path}` | GET | IME input method proxy (port 6091) |

#### Pod/Container Management

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/computer/pod/count` | GET | Container count statistics (grouped by service type) |
| `/computer/pod/list` | GET | List all container details (pagination: `?limit=100`) |
| `/computer/pod/ensure` | POST | Ensure container exists (idempotent, does not start Agent) |
| `/computer/pod/keepalive` | POST | Refresh container activity timestamp (prevents auto-cleanup) |
| `/computer/pod/restart` | POST | Restart container (destroy and recreate) |
| `/computer/pod/status` | GET | Query container status (`?user_id=xxx`) |
| `/computer/pod/vnc-status` | GET | Query VNC service readiness |

### Usage Examples

#### Health Check

```bash
curl -X GET http://localhost:8087/health
```

Response:
```json
{
  "status": "ok",
  "timestamp": "2024-01-01T00:00:00Z"
}
```

#### Chat

```bash
curl -X POST http://localhost:8087/chat \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "Help me create a Rust Web API project",
    "project_id": "my-project",
    "session_id": "optional-session-id"
  }'
```

#### Computer Agent Chat

```bash
curl -X POST http://localhost:8087/computer/chat \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user-123",
    "project_id": "my-project",
    "prompt": "Help me create a Python web application"
  }'
```

#### Real-time Progress Stream

```bash
curl -X GET http://localhost:8087/agent/progress/your-session-id \
  -H "Accept: text/event-stream"
```

## Project Structure

```
crates/
├── agent_abstraction/     # Agent abstraction layer
├── agent_config/          # Agent configuration management
├── agent_runner/          # Agent runtime (gRPC server)
│   ├── src/
│   │   ├── grpc/          # gRPC service implementation
│   │   ├── proxy_agent/   # ACP agent implementation
│   │   └── service/       # Core services
│   └── Cargo.toml
├── docker_manager/        # Docker container management
├── duckdb_manager/        # DuckDB database management
├── rcoder/               # Main application
│   ├── src/
│   │   ├── grpc/         # gRPC client
│   │   ├── handler/      # HTTP handlers
│   │   ├── service/      # Business services
│   │   └── cleanup_task/ # Cleanup tasks
│   └── Cargo.toml
├── rcoder-proxy/         # Pingora proxy wrapper
├── rcoder-telemetry/     # Telemetry and tracing
└── shared_types/         # Shared types and Proto definitions
    └── proto/
        └── agent.proto   # gRPC protocol definition
```

## Configuration

### Priority Order

1. **CLI arguments** - Highest priority
2. **Environment variables** - Medium priority
3. **Config file** - Lower priority
4. **Defaults** - Lowest priority

### Config File (config.yml)

```yaml
# Default Agent ID
default_agent_id: "claude-code-acp-ts"

# Project workspace directory
projects_dir: "./project_workspace"

# Main service port
port: 8087

# Docker configuration
docker_config:
  network_mode: "bridge"
  network_base_name: "agent-network"
  work_dir: "/app"
  auto_cleanup: true
  container_ttl_seconds: 3600
  api_timeout_seconds: 10
  cache_status_ttl_seconds: 10

# Container cleanup configuration
cleanup_config:
  enabled: true
  idle_timeout_seconds: 600
  cleanup_interval_seconds: 300
  container_protection_seconds: 300

# API Key authentication
api_key_auth:
  enabled: false
  api_key: "sk-xxx"

# Reverse proxy configuration
proxy_config:
  listen_port: 8088
  default_backend_port: 8086
  backend_host: "127.0.0.1"
```

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `RCODER_PORT` | Service port | 8087 |
| `RCODER_PROJECTS_DIR` | Projects directory | ./project_workspace |
| `RCODER_NETWORK_MODE` | Docker network mode | bridge |
| `RCODER_NETWORK_BASE_NAME` | Network base name | agent-network |
| `RCODER_API_TIMEOUT_SECONDS` | Docker API timeout | 10 |
| `RCODER_API_KEY_ENABLED` | Enable API Key auth | false |
| `RCODER_API_KEY` | API Key secret | - |
| `RUST_LOG` | Log level | info |

### Examples

```bash
# Using environment variables
RCODER_PORT=8080 RUST_LOG=debug cargo run --bin rcoder

# CLI arguments take highest priority
RCODER_PORT=8080 cargo run --bin rcoder -- --port 9000
```

## Development Guide

### Running Tests

```bash
# Run all tests
cargo test --workspace

# Run unit tests
make test-unit

# Run integration tests
make test-integration
```

### Code Quality

```bash
# Format code
cargo fmt

# Lint
cargo clippy

# Strict lint
cargo clippy -- -D warnings
```

### Local Development

```bash
# Start development server
RUST_LOG=debug cargo run --bin rcoder -- --port 8087

# Watch for file changes
cargo install cargo-watch
cargo watch -x "run --bin rcoder"
```

## Deployment

### Docker

```bash
# Build images
make docker-build

# Or build separately
make docker-build-master       # Main service image
make docker-build-agent-runner # Agent Runner image

# Production image (no debug tools)
make docker-build-agent-production
```

### Docker Compose

```bash
# Start services
make dev-up

# Check status
docker-compose -f docker/docker-compose.yml ps

# Stop services
make dev-down
```

### Pyroscope Profiling

```bash
# Start Pyroscope Server
make pyroscope-up

# Open Web UI
open http://localhost:4040

# Stop service
make pyroscope-down
```

## Troubleshooting

### Common Issues

- **Port already in use** - Use `--port` to specify a different port
- **Container startup failure** - Check Docker service status and network configuration
- **gRPC connection failure** - Verify container network and port configuration
- **API Key error** - Check `api_key_auth` configuration

### Debug Mode

```bash
# Enable verbose logging
RUST_LOG=debug cargo run --bin rcoder

# View container logs
make dev-logs

# Enter container for debugging
docker exec -it <container_id> /bin/bash
```

## Changelog

### v0.1.0 (Current)

#### New Features
- SACP protocol-based unified AI agent management
- gRPC high-performance communication architecture
- Docker containerized deployment (project-level isolation)
- VNC/noVNC remote desktop support
- API Key authentication middleware
- Automatic container cleanup
- Multi-image configuration support
- Pyroscope profiling integration

#### Technical Highlights
- Rust 2024 Edition
- Tonic gRPC (v0.14.2)
- SACP Protocol (v10.1.0)
- MCP Protocol support (rmcp v0.12.0)
- DuckDB database
- OpenTelemetry tracing
- eBPF debugging tool support

## Links

- **Repository**: [GitHub](https://github.com/your-org/rcoder)
- **Issue Tracker**: [Issues](https://github.com/your-org/rcoder/issues)
- **SACP Protocol**: [Symposium ACP](https://crates.io/crates/sacp)
- **MCP Protocol**: [rmcp](https://crates.io/crates/rmcp)

## License

This project is dual-licensed under MIT or Apache-2.0. See [LICENSE](LICENSE) for details.

## Contributing

Contributions are welcome! Please read [CONTRIBUTING.md](CONTRIBUTING.md) to learn how to get involved.

1. Fork the project
2. Create your feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'Add amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request

---

**Built by the RCoder team, dedicated to advancing AI-powered modern development experiences.**
