# MCP Shared Strategy: Centralized / Network-Based
> **Status**: Implemented (Requires Image Rebuild)

## Problem
The user wants a strategy to **share** the `mcp-proxy` (Chrome) service efficiently, avoiding the resource waste of running it in every container (OOM issue), while still having it available (not simply disabled).

## Solution: Network-Based Service Discovery

We have decoupled the **Configuration** (where to connect) from the **Topology** (where it runs).

### 1. Hostname Alias (`mcp-proxy-host`)
The Agent code (`computer_agent_default.json`) now connects to:
`http://mcp-proxy-host:18099`
Instead of hardcoded `127.0.0.1`.

### 2. Startup Logic (`start-up.sh`)
The container startup script dynamically configures networking based on `ENABLE_MCP_PROXY` env var.

**Scenario A: Local Mode (Default)**
- `ENABLE_MCP_PROXY="true"`
- Script:
    1.  Starts local processes (`mcp-proxy` + Chrome).
    2.  Modifies `/etc/hosts`: `127.0.0.1 mcp-proxy-host`.
- Result: Self-contained, zero-configuration usage.

**Scenario B: Shared/Centralized Mode**
- `ENABLE_MCP_PROXY="false"`
- Script:
    1.  Does **NOT** start local processes (Saves RAM).
    2.  Does **NOT** modify `/etc/hosts`.
- Requirement: The container must be able to resolve `mcp-proxy-host` via DNS.
- Implementation: User defines a service named `mcp-proxy-host` in `docker-compose.yml`.

### Example Docker Compose (Shared Mode)
```yaml
services:
  # The Shared Service (One instance)
  mcp-proxy-host:
    image: rcoder-agent-runner:latest
    entrypoint: ["/bin/sh", "-c", "start_mcp_proxy_services && wait"] # Simplified

  # The Agents (Many instances)
  agent-1:
    image: rcoder-agent-runner:latest
    environment:
      ENABLE_MCP_PROXY: "false" # Don't start local
    depends_on:
      - mcp-proxy-host
```

## Benefits
- **Flexibility**: Zero code change to switch between Local and Centralized.
- **Resource Efficiency**: Run 1 Chrome for 50 Agents.
- **Backward Compatibility**: Defaults to Local behavior.
