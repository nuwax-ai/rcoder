# ============================================================================
# DevSpace Development Mode
# ============================================================================

.PHONY: devspace-init devspace-dev devspace-build devspace-down devspace-purge devspace-logs devspace-enter devspace-run devspace-help k8s-run run-in-container

# Initialize DevSpace (first time)
devspace-init:
	@echo "Initializing DevSpace development environment..."
	@devspace init --no-input
	@echo "DevSpace initialization completed"
	@echo "Next step: make devspace-dev to start development mode"

# Start DevSpace development mode
devspace-dev:
	@echo "Starting DevSpace development mode..."
	@echo "Auto-syncing files to container"
	@devspace dev --namespace=rcoder-dev

# Build DevSpace image
devspace-build:
	@echo "Building DevSpace image..."
	@devspace build --namespace=rcoder-dev

# Stop DevSpace
devspace-down:
	@echo "Stopping DevSpace..."
	@devspace purge --namespace=rcoder-dev

# Clean up DevSpace resources
devspace-purge:
	@echo "Cleaning up DevSpace resources..."
	@devspace purge --namespace=rcoder-dev
	@kubectl delete namespace rcoder-dev --ignore-not-found
	@echo "DevSpace resources cleaned up"

# View DevSpace logs
devspace-logs:
	@echo "Viewing DevSpace logs..."
	@devspace logs --namespace=rcoder-dev

# Enter container
devspace-enter:
	@echo "Entering container..."
	@devspace enter --namespace=rcoder-dev

# Install system dependencies
devspace-install-deps:
	@echo "Installing system dependencies..."
	@devspace run install-deps

# Run rcoder service in container (auto compile and run)
devspace-run:
	@echo "Running rcoder service in container..."
	@devspace run run

# Compile rcoder in container
devspace-build-local:
	@echo "Compiling rcoder in container..."
	@devspace run build

# Run tests in container
devspace-test:
	@echo "Running tests in container..."
	@devspace run test

# Run rcoder service in K8s container (simplified command)
k8s-run:
	@echo "Running rcoder service in K8s container..."
	@devspace enter --namespace=rcoder-dev -- bash -c 'cd /app && CONTAINER_RUNTIME=kubernetes cargo run --bin rcoder --features ebpf-debug,pyroscope,otel,debug,kubernetes -- --port 8290'

# Run rcoder service in container (execute inside container)
run-in-container:
	@echo "Stopping existing rcoder processes..."
	@pkill rcoder 2>/dev/null || true
	@sleep 2
	@echo "Starting rcoder service in container..."
	@cd /app && CONTAINER_RUNTIME=kubernetes cargo run --bin rcoder --features ebpf-debug,pyroscope,otel,debug,kubernetes -- --port 8290

# Display DevSpace help
devspace-help:
	@echo "DevSpace Development Commands:"
	@echo ""
	@echo "  make devspace-init         - Initialize DevSpace (first time)"
	@echo "  make devspace-dev          - Start development mode (auto sync)"
	@echo "  make devspace-build        - Build DevSpace image"
	@echo "  make devspace-down         - Stop DevSpace"
	@echo "  make devspace-purge        - Clean up all DevSpace resources"
	@echo "  make devspace-logs         - View logs"
	@echo "  make devspace-enter        - Enter container (interactive)"
	@echo "  make devspace-install-deps - Install system dependencies"
	@echo "  make devspace-run          - Run rcoder service in container"
	@echo "  make devspace-build-local  - Compile rcoder in container"
	@echo "  make devspace-test         - Run tests in container"
	@echo "  make k8s-run               - Run rcoder service in K8s container"
	@echo "  make run-in-container      - Run rcoder service in container (execute inside)"
	@echo ""
	@echo "Development workflow (first time):"
	@echo "  1. make devspace-dev         # Start development mode (auto sync)"
	@echo "  2. make devspace-install-deps # Install system dependencies (first time)"
	@echo "  3. make k8s-run              # Run rcoder service in K8s container"
	@echo "  4. Modify code               # Auto sync to container"
	@echo "  5. Ctrl+C                    # Stop service"
	@echo ""
	@echo "  Or:"
	@echo "  1. make devspace-dev         # Start development mode"
	@echo "  2. make devspace-enter       # Enter container"
	@echo "  3. make run-in-container     # Run rcoder service in container"
