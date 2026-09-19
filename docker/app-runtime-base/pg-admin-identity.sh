#!/bin/sh
# PGDATA-local initialization identity. Contains a username, never a password.
# Source before initdb; only record after initdb or a verified superuser login.
pg_admin_identity_load() {
    PG_ADMIN_IDENTITY_FILE="$PGDATA/.rcoder-admin-user"
    if [ -e "$PG_ADMIN_IDENTITY_FILE" ]; then
        PG_ADMIN_USER=$(cat "$PG_ADMIN_IDENTITY_FILE") || return 1
        if [ -n "${APP_PG_ADMIN_USER:-}" ] && [ "$APP_PG_ADMIN_USER" != "$PG_ADMIN_USER" ]; then
            echo '[pg] configured administrator differs from PGDATA identity' >&2
            return 1
        fi
    else
        PG_ADMIN_USER=${APP_PG_ADMIN_USER:-$POSTGRES_USER}
    fi
    case "$PG_ADMIN_USER" in
        ''|[!a-zA-Z_]*|*[!a-zA-Z0-9_]*)
            echo '[pg] invalid PGDATA administrator identity' >&2; return 1 ;;
    esac
    if [ "${#PG_ADMIN_USER}" -gt 63 ]; then
        echo '[pg] administrator identity is too long' >&2; return 1
    fi
}

pg_admin_identity_record() {
    if [ -e "$PG_ADMIN_IDENTITY_FILE" ]; then
        _pg_existing=$(cat "$PG_ADMIN_IDENTITY_FILE") || return 1
        [ "$_pg_existing" = "$PG_ADMIN_USER" ] || return 1
        return 0
    fi
    _pg_identity_tmp=$(mktemp "$PGDATA/.rcoder-admin-user.XXXXXX") || return 1
    if ! printf '%s\n' "$PG_ADMIN_USER" > "$_pg_identity_tmp"; then
        rm -f "$_pg_identity_tmp"; return 1
    fi
    # link is atomic and cannot overwrite an identity published by another task.
    if ln "$_pg_identity_tmp" "$PG_ADMIN_IDENTITY_FILE" 2>/dev/null; then
        rm -f "$_pg_identity_tmp"
        return 0
    fi
    rm -f "$_pg_identity_tmp"
    _pg_existing=$(cat "$PG_ADMIN_IDENTITY_FILE") || return 1
    [ "$_pg_existing" = "$PG_ADMIN_USER" ]
}


# stdin variables are quoted by psql, never interpolated into SQL identifiers.
pg_bootstrap_database() {
    _pg_ready=0
    for _pg_attempt in $(seq 1 300); do
        _pg_superuser=$(env -u PGHOSTADDR -u PGSERVICE PGCONNECT_TIMEOUT=2 PGOPTIONS='-c statement_timeout=2000' \
            "$PG_BIN/psql" -X -w -h /var/run/postgresql -U "$PG_ADMIN_USER" \
            -d postgres -tAc "SELECT 1 FROM pg_roles WHERE rolname=current_user AND rolsuper" 2>/dev/null) || _pg_superuser=
        if [ "$_pg_superuser" = 1 ]; then
            _pg_ready=1
            break
        fi
        sleep 1
    done
    if [ "$_pg_ready" != 1 ]; then
        echo '[pg] administrator login could not be verified' >&2
        return 1
    fi
    pg_admin_identity_record || return 1
    _pg_exists=$(pg_database_exists) || return 1
    if [ "$_pg_exists" = 1 ]; then
        return 0
    fi
    if ! env -u PGHOSTADDR -u PGSERVICE PGCONNECT_TIMEOUT=5 \
        "$PG_BIN/createdb" -w -h /var/run/postgresql -U "$PG_ADMIN_USER" -- "$POSTGRES_DB"; then
        # Only a confirmed database is an idempotent success after a race.
        _pg_exists=$(pg_database_exists) || return 1
        [ "$_pg_exists" = 1 ] || return 1
    fi
    _pg_exists=$(pg_database_exists) || return 1
    [ "$_pg_exists" = 1 ]
}

pg_database_exists() {
    env -u PGHOSTADDR -u PGSERVICE PGCONNECT_TIMEOUT=2 PGOPTIONS='-c statement_timeout=2000' \
        "$PG_BIN/psql" -X -w -h /var/run/postgresql -U "$PG_ADMIN_USER" \
        -d postgres -v ON_ERROR_STOP=1 -v requested_db="$POSTGRES_DB" -tA <<'SQL'
SELECT 1 FROM pg_database WHERE datname = :'requested_db';
SQL
}
