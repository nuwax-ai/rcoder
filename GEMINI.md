# Gemini Context: RCoder Project

This document provides a comprehensive overview of the `rcoder` project, its architecture, and development conventions to be used as instructional context for Gemini.

## 1. Project Overview

**RCoder** is a sophisticated, Rust-based AI-driven development platform. It functions as a central orchestrator that can manage and communicate with various AI coding agents. The system is designed with a modern microservices-style architecture, leveraging containerization for both its own components and the agents it manages.

### Key Technologies

- **Language:** Rust (2021 Edition, Workspace)
- **Backend Frameworks:**
    - **HTTP:** `axum` (for the main REST API and SSE)
    - **gRPC:** `tonic` (for internal service-to-service communication)
- **Reverse Proxy:** `pingora` for high-performance routing.
- **Containerization:** Docker & Docker Compose.
- **Core Libraries:** `tokio` (async runtime), `serde` (serialization), `clap` (CLI parsing), `tracing` (structured logging), `bollard` (Docker API interaction).

### Architecture

The project consists of two primary services, typically run in Docker containers:

1.  **`rcoder` (HTTP Gateway / Orchestrator):**
    *   Exposes a public REST API for clients (e.g., `/chat`).
    *   Provides real-time progress updates via Server-Sent Events (SSE).
    *   Acts as a gRPC client, forwarding requests to the `agent_runner`.
    *   Manages the lifecycle of agent containers using the Docker daemon (via a mounted Docker socket).
    *   Includes an optional `pingora` reverse proxy for flexible routing to other local services.

2.  **`agent_runner` (gRPC Service / Worker):**
    *   The core backend service that exposes a gRPC API defined in `agent.proto`.
    *   Receives tasks from the `rcoder` service.
    *   Executes the actual AI agent logic.
    *   Streams progress events back to `rcoder` via a gRPC server-streaming RPC.
    *   Note: The `agent_runner` binary can run in different modes. It is used as the executable for both the `rcoder` (full mode) and `agent_runner` (agent-only mode) services.

### Communication

-   **External:** Clients communicate with the `rcoder` service via a standard REST API and consume a Server-Sent Events (SSE) stream for progress.
-   **Internal:** The `rcoder` gateway communicates with the `agent_runner` service using a well-defined, type-safe gRPC API. The API contract is located at `crates/shared_types/proto/agent.proto`.

---

## 2. Building and Running

The project is designed to be run within a containerized environment using Docker Compose. The `Makefile` provides convenient scripts for the entire development lifecycle.

### Primary Workflow (Docker)

The recommended workflow for development is:

1.  **First-time Setup:** Build the main Docker image.
    ```bash
    make dev-build
    ```

2.  **Start Services:** Launch the services using Docker Compose.
    ```bash
    make dev-up
    ```

3.  **View Logs:** Tail the logs from the running services.
    ```bash
    make dev-logs
    ```

4.  **Make Code Changes:** After modifying the Rust source code, restart the services. This command quickly rebuilds the image and restarts the containers.
    ```bash
    make dev-restart
    ```

5.  **Stop Services:** Shut down the Docker Compose environment.
    ```bash
    make dev-down
    ```

### Local (Non-Docker) Builds

While the primary workflow is container-based, you can also build and install the binaries locally.

-   **Build release binaries:**
    ```bash
    cargo build --release --workspace
    ```
    (The main binary is `target/release/rcoder`)

-   **Install binaries to `~/.cargo/bin`:**
    ```bash
    make install
    ```

---

## 3. Development Conventions

-   **Workspace Structure:** The project is a Rust workspace located in the `crates/` directory. Shared types, especially gRPC definitions, are in the `crates/shared_types` crate.
-   **Configuration:** The system uses a layered configuration approach (CLI arguments > Environment Variables > `config.yml`), with the base configuration defined in `config.yml`.
-   **API First Design:**
    -   Internal APIs are defined using Protobuf in `crates/shared_types/proto/agent.proto`. Any changes to internal communication should start here.
    -   External APIs are RESTful, with routes defined in `crates/rcoder/src/router.rs`.
-   **Code Style:** The project follows standard Rust conventions. Use the following commands to maintain code quality:
    ```bash
    # Format the entire workspace
    cargo fmt

    # Lint the entire workspace
    cargo clippy --workspace --all-targets
    ```
-   **Containerization:** The application is container-aware. It interacts directly with the Docker socket to manage other containers. The `docker-compose.yml` and `docker/Dockerfile` files define the development and production environments.
-   **Logging:** Structured logging is implemented via the `tracing` crate. Logs are output to both the console and to rolling files in the `logs/` directory inside the container (which is volume-mounted to the host).
