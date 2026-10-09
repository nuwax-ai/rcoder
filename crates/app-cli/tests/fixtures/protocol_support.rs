//! Shared construction for the explicit protocol fixture; no confirmation skip.
use std::path::{Path, PathBuf};

pub fn write_pingap(root: &Path) -> PathBuf {
    let path = root.join("protocol-pingap");
    let binary = env!("CARGO_BIN_EXE_tree-fixture").replace('\'', "'\"'\"'");
    std::fs::write(
        &path,
        format!("#!/bin/sh\nexec '{binary}' protocol-pingap \"$@\"\n"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

pub fn write_listener_override(workspace: &Path, service: &str) {
    if workspace.join("protocol-listener.toml").is_file() {
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    std::fs::write(workspace.join("protocol-listener.toml"), format!("[servers.app]\naddr='0.0.0.0:9080,{address}'\nlocations=['business']\n[locations.business]\npath='/'\nupstream='{service}'\n[upstreams.\"{service}\"]\naddrs=['rcoder://{service}']\n")).unwrap();
}

pub fn reserve_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}
