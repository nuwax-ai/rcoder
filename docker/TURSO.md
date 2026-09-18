# userApp SQLite persistence

This Compose configuration uses SQLite for userApp only. Agent storage is configured independently. Run one rcoder instance per data directory. Kubernetes uses PostgreSQL instead.

The default host directory is `./data/rcoder`, resolved relative to this Compose file. Set `RCODER_DATA_DIR` to a dedicated absolute directory to override it. The entire directory is mounted at `/app/data`; the database is `/app/data/userapp.sqlite3`. WAL, SHM and the instance lock stay beside it. Application purge does not include this directory.

Startup creates the database directory and checks access by the actual service user, exclusive instance ownership, local filesystem support and migrations. Errors stop startup. Do not recursively change ownership of existing application data. A custom host directory must already be writable by the configured container user.

Database files, WAL/SHM/journal sidecars and `.userapp-instance.lock` must be ordinary files without symbolic links or hard-link aliases. A symlink to the data directory is resolved before locking and opening the database, so alternate directory paths cannot start a second instance. Keep the directory private to this deployment; do not replace its files or add links while rcoder is running.

Use a local filesystem. NFS/SMB are unsupported. On macOS, the default bind mount is suitable for local verification when the filesystem checks pass. If it is rejected, use:

```bash
docker compose -f docker-compose.yml -f docker-compose.sqlite-volume.yml up -d rcoder
```

The named volume is project-scoped. `down` and service recreation preserve it; `down -v` deletes it. Never use `down -v` on an instance whose data must be retained. Do not share the volume between Compose projects.

For backups, use SQLite's consistent online backup API, or stop rcoder and copy the entire data directory before restarting it. Never copy only the `.db`/`.sqlite3` file from a running instance. Restore into an isolated directory while rcoder is stopped; keep the original backup until migration and health checks succeed.

The rcoder binary must include the `userapp-sqlite` feature (enabled by default). The SQLite configuration alone does not prove that a previously built image supports persistence. After building and starting the new image, inspect the rcoder mount and environment, check `/health`, and test identity retention after recreating only the rcoder container. Test runs must use a run-specific directory and must not erase this development instance's data.

For local development, use `make dev-restart` for image changes and `make dev-hot` for Rust changes. There is no `make dev-host` target. Both build paths retain the default `userapp-sqlite` feature. Before building the builder image, its four startup scripts are compared with the available `build-agent-docker` checkout. A mismatch stops the build without overwriting either worktree; review and synchronize the intended changes explicitly. Local SQLite data directories are excluded from the Docker build context as well as Git.
