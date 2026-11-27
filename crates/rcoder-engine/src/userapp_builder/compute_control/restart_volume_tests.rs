//! 重启之后挂载见证的比对：只放行替代容器自己记下的那几条换源——旧目录、新目录、是哪一个
//! 容器都要对得上；其余一律严格核对。
use super::*;

const REPLACEMENT: &str = "replacement-container";
const IDENTITY_CHANGED: &str = "Builder workspace volume identity changed during restart";
const NOT_THE_RECORDED_CONTAINER: &str =
    "Restart target was replaced before its mounts could be verified";
const UNEXPECTED_SWAP: &str = "Builder restart recorded";

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
const ANOTHER_LOG: Mount = Mount {
    uid: "/srv/logs/container/runner-older",
    ..LOG
};
const WORKSPACE_ELSEWHERE: Mount = Mount {
    uid: "/srv/elsewhere/workspace",
    ..WORKSPACE
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

/// 替代容器 `container` 记下的换源见证：`swaps` 里的每一条从旧目录换到新目录
fn witness(container: &str, swaps: &[(&Mount, &Mount)]) -> BuilderRestartMountWitness {
    BuilderRestartMountWitness {
        container_id: container.to_owned(),
        replaced: swaps
            .iter()
            .map(|(old, new)| ReplacedBuilderMount {
                target: old.name.to_owned(),
                previous_source: old.uid.to_owned(),
                source: new.uid.to_owned(),
            })
            .collect(),
    }
}

fn verify(
    original: &[&Mount],
    prepared: &[&Mount],
    witness: Option<&BuilderRestartMountWitness>,
    replacement_id: Option<&str>,
) -> Result<()> {
    verify_builder_volume_witness(
        &checkpoint(original),
        &checkpoint(prepared),
        witness,
        replacement_id,
    )
}

fn refused(result: Result<()>, expected: &str) {
    assert!(
        matches!(&result, Err(error) if format!("{error:#}").contains(expected)),
        "expected {expected:?} in {result:?}"
    );
}

#[test]
fn identical_mounts_pass_without_any_witness() {
    assert!(verify(&[&WORKSPACE, &LOG], &[&WORKSPACE, &LOG], None, None).is_ok());
    assert!(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE, &LOG],
            Some(&witness(REPLACEMENT, &[])),
            Some(REPLACEMENT)
        )
        .is_ok()
    );
}

/// 见证记下的那一条换源，对得上就放行
#[test]
fn the_recorded_swap_of_the_recorded_container_passes() {
    assert!(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE, &FRESH_LOG],
            Some(&witness(REPLACEMENT, &[(&LOG, &FRESH_LOG)])),
            Some(REPLACEMENT)
        )
        .is_ok()
    );
}

/// 重新采集到的不是见证记的那个容器：见证不作数
#[test]
fn a_witness_only_counts_for_its_own_container() {
    let witness = witness(REPLACEMENT, &[(&LOG, &FRESH_LOG)]);
    refused(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE, &FRESH_LOG],
            Some(&witness),
            Some("another-container"),
        ),
        NOT_THE_RECORDED_CONTAINER,
    );
    refused(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE, &FRESH_LOG],
            Some(&witness),
            None,
        ),
        NOT_THE_RECORDED_CONTAINER,
    );
}

/// 换过去的不是见证记的那个目录：不行。没换、换回来了也不行——见证和实际挂载对不上
#[test]
fn the_new_source_must_be_the_recorded_one() {
    let witness = witness(REPLACEMENT, &[(&LOG, &FRESH_LOG)]);
    refused(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE, &ANOTHER_LOG],
            Some(&witness),
            Some(REPLACEMENT),
        ),
        IDENTITY_CHANGED,
    );
    refused(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE, &LOG],
            Some(&witness),
            Some(REPLACEMENT),
        ),
        IDENTITY_CHANGED,
    );
}

#[test]
fn a_witness_is_refused_when_its_record_does_not_match_the_archive() {
    // 旧目录不是归档里的那个
    refused(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE, &FRESH_LOG],
            Some(&witness(REPLACEMENT, &[(&ANOTHER_LOG, &FRESH_LOG)])),
            Some(REPLACEMENT),
        ),
        UNEXPECTED_SWAP,
    );
    // 点名的挂载归档里没有
    refused(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE, &FRESH_LOG],
            Some(&witness(
                REPLACEMENT,
                &[(&LOG, &FRESH_LOG), (&WORKSPACE_ELSEWHERE, &FRESH_LOG)],
            )),
            Some(REPLACEMENT),
        ),
        UNEXPECTED_SWAP,
    );
    // 同一条挂载点名两次
    refused(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE, &FRESH_LOG],
            Some(&witness(
                REPLACEMENT,
                &[(&LOG, &FRESH_LOG), (&LOG, &ANOTHER_LOG)],
            )),
            Some(REPLACEMENT),
        ),
        UNEXPECTED_SWAP,
    );
    // 点名的不是 HostPath 挂载
    let volume = Mount {
        kind: AppResourceKind::PersistentVolumeClaim,
        ..LOG
    };
    refused(
        verify(
            &[&WORKSPACE, &volume],
            &[&WORKSPACE, &FRESH_LOG],
            Some(&witness(REPLACEMENT, &[(&volume, &FRESH_LOG)])),
            Some(REPLACEMENT),
        ),
        UNEXPECTED_SWAP,
    );
}

/// 持久数据（`/home/user` 下面）谁点名都不换：运行时报上来一份"工作区换了源"的见证，引擎拒绝
#[test]
fn persistent_data_is_never_swapped_whatever_the_runtime_reports() {
    for workspace in [
        &WORKSPACE,
        &Mount {
            name: "/home/user",
            ..WORKSPACE
        },
        &Mount {
            name: "/home/user/data",
            ..WORKSPACE
        },
    ] {
        refused(
            verify(
                &[workspace, &LOG],
                &[&WORKSPACE_ELSEWHERE, &LOG],
                Some(&witness(REPLACEMENT, &[(workspace, &WORKSPACE_ELSEWHERE)])),
                Some(REPLACEMENT),
            ),
            UNEXPECTED_SWAP,
        );
    }
}

#[test]
fn anything_not_recorded_is_refused() {
    // 没有见证，换了源
    refused(
        verify(&[&WORKSPACE, &LOG], &[&WORKSPACE, &FRESH_LOG], None, None),
        IDENTITY_CHANGED,
    );
    // 见证只点了日志挂载，变的却是工作区
    refused(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE_ELSEWHERE, &FRESH_LOG],
            Some(&witness(REPLACEMENT, &[(&LOG, &FRESH_LOG)])),
            Some(REPLACEMENT),
        ),
        IDENTITY_CHANGED,
    );
}

#[test]
fn a_replaced_mount_may_only_change_its_source() {
    let kind = Mount {
        kind: AppResourceKind::PersistentVolumeClaim,
        ..FRESH_LOG
    };
    refused(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE, &kind],
            Some(&witness(REPLACEMENT, &[(&LOG, &FRESH_LOG)])),
            Some(REPLACEMENT),
        ),
        IDENTITY_CHANGED,
    );
    let name = Mount {
        name: "/app/other-logs",
        ..FRESH_LOG
    };
    refused(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE, &name],
            Some(&witness(REPLACEMENT, &[(&LOG, &FRESH_LOG)])),
            Some(REPLACEMENT),
        ),
        IDENTITY_CHANGED,
    );
    let version = Mount {
        version: "log-v2",
        ..FRESH_LOG
    };
    refused(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE, &version],
            Some(&witness(REPLACEMENT, &[(&LOG, &FRESH_LOG)])),
            Some(REPLACEMENT),
        ),
        IDENTITY_CHANGED,
    );
    // 版本变了、源没变：同样不行
    let unchanged_source = Mount {
        uid: LOG.uid,
        ..version
    };
    refused(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE, &unchanged_source],
            Some(&witness(REPLACEMENT, &[(&LOG, &FRESH_LOG)])),
            Some(REPLACEMENT),
        ),
        IDENTITY_CHANGED,
    );
}

#[test]
fn the_count_and_the_order_must_not_change() {
    let witness = witness(REPLACEMENT, &[(&LOG, &FRESH_LOG)]);
    refused(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE],
            Some(&witness),
            Some(REPLACEMENT),
        ),
        IDENTITY_CHANGED,
    );
    refused(
        verify(
            &[&WORKSPACE, &LOG],
            &[&WORKSPACE, &LOG, &FRESH_LOG],
            Some(&witness),
            Some(REPLACEMENT),
        ),
        IDENTITY_CHANGED,
    );
    refused(
        verify(
            &[&WORKSPACE, &LOG],
            &[&FRESH_LOG, &WORKSPACE],
            Some(&witness),
            Some(REPLACEMENT),
        ),
        IDENTITY_CHANGED,
    );
}
