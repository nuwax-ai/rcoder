//! 重启之后挂载见证的比对：只放行运行时点名的那几条挂载换源，其余一律严格核对。
use super::*;

const REFUSED: &str = "Builder workspace volume identity changed during restart";

struct Mount {
    kind: AppResourceKind,
    name: &'static str,
    uid: &'static str,
    version: &'static str,
}

const WORKSPACE: Mount = Mount {
    kind: AppResourceKind::HostPath,
    name: "/home/user/userapp",
    uid: "/srv/workspace",
    version: "workspace-v1",
};
const LOG: Mount = Mount {
    kind: AppResourceKind::HostPath,
    name: "/app/container-logs",
    uid: "/srv/logs/container/runner-old",
    version: "log-v1",
};
const FRESH_LOG: Mount = Mount {
    uid: "/srv/logs/container/runner-new",
    ..LOG
};

fn identity(mount: &Mount) -> AppResourceIdentity {
    AppResourceIdentity {
        kind: mount.kind,
        name: mount.name.to_owned(),
        uid: mount.uid.to_owned(),
        resource_version: Some(mount.version.to_owned()),
    }
}

fn checkpoint(mounts: &[&Mount]) -> serde_json::Value {
    serde_json::json!({ "builder_volumes": mounts.iter().map(|mount| identity(mount)).collect::<Vec<_>>() })
}

fn verify(original: &[&Mount], prepared: &[&Mount], replaced: &[&str]) -> Result<()> {
    let replaced: Vec<String> = replaced.iter().map(|name| (*name).to_owned()).collect();
    verify_builder_volume_witness(&checkpoint(original), &checkpoint(prepared), &replaced)
}

fn refused(result: Result<()>) {
    assert!(
        matches!(&result, Err(error) if format!("{error:#}").contains(REFUSED)),
        "{result:?}"
    );
}

#[test]
fn identical_mounts_pass() {
    assert!(verify(&[&WORKSPACE, &LOG], &[&WORKSPACE, &LOG], &[]).is_ok());
    // 点名了但没有变，也照常通过
    assert!(verify(&[&WORKSPACE, &LOG], &[&WORKSPACE, &LOG], &[LOG.name]).is_ok());
}

#[test]
fn the_source_of_a_named_mount_may_change() {
    assert!(verify(&[&WORKSPACE, &LOG], &[&WORKSPACE, &FRESH_LOG], &[LOG.name]).is_ok());
}

#[test]
fn a_changed_source_is_refused_unless_the_runtime_named_the_mount() {
    // 没点名
    refused(verify(&[&WORKSPACE, &LOG], &[&WORKSPACE, &FRESH_LOG], &[]));
    // 点名的是工作区，变的却是日志挂载
    refused(verify(
        &[&WORKSPACE, &LOG],
        &[&WORKSPACE, &FRESH_LOG],
        &[WORKSPACE.name],
    ));
}

#[test]
fn a_named_mount_may_only_change_its_source() {
    let kind = Mount {
        kind: AppResourceKind::PersistentVolumeClaim,
        ..FRESH_LOG
    };
    refused(verify(
        &[&WORKSPACE, &LOG],
        &[&WORKSPACE, &kind],
        &[LOG.name],
    ));
    let name = Mount {
        name: "/app/other-logs",
        ..FRESH_LOG
    };
    refused(verify(
        &[&WORKSPACE, &LOG],
        &[&WORKSPACE, &name],
        &[LOG.name],
    ));
    let version = Mount {
        version: "log-v2",
        ..FRESH_LOG
    };
    refused(verify(
        &[&WORKSPACE, &LOG],
        &[&WORKSPACE, &version],
        &[LOG.name],
    ));
    // 版本变了、源没变：同样不行
    let unchanged_source = Mount {
        uid: LOG.uid,
        ..version
    };
    refused(verify(
        &[&WORKSPACE, &LOG],
        &[&WORKSPACE, &unchanged_source],
        &[LOG.name],
    ));
}

#[test]
fn the_count_and_the_order_must_not_change() {
    refused(verify(&[&WORKSPACE, &LOG], &[&WORKSPACE], &[LOG.name]));
    refused(verify(
        &[&WORKSPACE, &LOG],
        &[&WORKSPACE, &LOG, &FRESH_LOG],
        &[LOG.name],
    ));
    refused(verify(
        &[&WORKSPACE, &LOG],
        &[&FRESH_LOG, &WORKSPACE],
        &[LOG.name],
    ));
}
