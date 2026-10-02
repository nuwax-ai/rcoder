//! Backend-independent lifecycle transactions. All calls run inside one owned tx.
use super::super::{domain, storage};
use super::{codec, configuration, repo};
use crate::db::{models, schema::Backend};
use chrono::SubsecRound as _;
use shared_types::*;
use toasty::Executor;
type Error = UserAppStoreError;

// 拆分（file-server 大文件范式）：`application` 身份/元数据/受理 /
// `operation` 操作读取推进与租约 / `recovery` 恢复收敛、资源绑定与重建 /
// `observed` 观察终态收敛。原 pub(super) 入口统一升 pub(crate) 并经 glob
// 重导出，`common::ops::X` 旧路径保持可达；`validate_input` 为
// application/operation 共用校验留在本文件（原私可见性不变）。

fn validate_input(
    kind: UserAppOperationKind,
    fingerprint: &str,
    command: Option<&UserAppControlCommand>,
    input: Option<&UserAppExecutionInput>,
) -> Result<(), Error> {
    match (command, input) {
        (None, Some(input))
            if kind == UserAppOperationKind::AdoptBuilder && fingerprint == input.digest() =>
        {
            Ok(())
        }
        (None, _) if kind == UserAppOperationKind::AdoptBuilder => Err(Error::InvalidOperation(
            "Adoption requires its original input digest".into(),
        )),
        (
            Some(
                UserAppControlCommand::Create { input_digest }
                | UserAppControlCommand::Update { input_digest }
                | UserAppControlCommand::Deploy { input_digest, .. },
            ),
            Some(input),
        ) if *input_digest == input.digest() => Ok(()),
        (
            Some(
                UserAppControlCommand::Create { .. }
                | UserAppControlCommand::Update { .. }
                | UserAppControlCommand::Deploy { .. },
            ),
            _,
        )
        | (_, Some(_)) => Err(Error::InvalidOperation(
            "Private execution input does not match command digest".into(),
        )),
        (_, None) => Ok(()),
    }
}

mod application;
mod deletion_recovery;
mod observed;
mod operation;
mod recovery;

pub(crate) use application::*;
pub(crate) use deletion_recovery::*;
pub(crate) use observed::*;
pub(crate) use operation::*;
pub(crate) use recovery::*;
