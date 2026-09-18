# 存量应用定点修复：devbuild 冻结安装参数

适用对象：由旧版模板（`--frozen-lockfile` 生成器，template-cli < 本轮修复版本）创建、
且 `[devbuild]` 仍带冻结参数的已有 UserApp。模板包升级**不会**更新已有应用的 manifest；
每个受影响应用需按下述步骤定点修复。背景见同目录 `spec.md` 与
`../../.zcode/plans/pnpm-frozen-lockfile-analysis.md`。

## 判定是否受影响

- dev build 失败且日志含 `ERR_PNPM_NO_LOCKFILE`（无 lockfile）或
  `ERR_PNPM_OUTDATED_LOCKFILE`（lockfile 与 package.json 不一致）；
- 该服务的 `project.manifest.toml` `[devbuild]` command 含 `pnpm install --frozen-lockfile`。

仅命令含冻结参数的应用需要处理；出现其他错误码的按各自原因排查，不做本流程。

## 定点修复步骤

1. **核对身份，不做推断**：经应用管理接口/持久存储确认 app_id、workspace 源码位置
   与实际失败的服务（以日志中的 service_id 为准）。不能凭 Pod 名、端口或"最近改过的
   应用"决定修改对象。
2. **记录现场**：读取该服务 `project.manifest.toml` 原文并保存（或小范围 diff），
   确认当前无并发构建/编辑任务（dev build 任务不在 Running 态）。
3. **只改冻结片段**：把 `[devbuild]` command 中的 `pnpm install --frozen-lockfile`
   改为 `pnpm install --no-frozen-lockfile`。
   - React 形态必须保留其后的 `&& pnpm run type-check`（或该应用原有的自定义检查命令）；
   - 用户自定义的前置/后置命令、其他段（build/run/devrun/proxy/health）一律不动；
   - 禁止全文件/全应用搜索替换，禁止删除 `pnpm-lock.yaml`、源码或数据卷。
4. **写入持久源码**：修改必须落在该应用的持久化 workspace 源码（经平台源码编辑/保存
   链路，或应用归属者自己的提交流程）；只改容器临时目录或只改本地产物不算完成。
5. **重跑验证**：重新触发开发构建（dev/start）。确认：
   - 安装成功，`pnpm-lock.yaml` 生成（或与 package.json 同步更新）；
   - 后续检查命令（如 type-check）确实执行；
   - 任务终态成功后服务可启动。
6. **记录证据**：app_id、修改 diff、构建任务结果与日志位置，归档到该环境的验证记录。

## 边界

- 不以重建应用、重跑脚手架作为常规修复手段（会引入无关变更）；
- 不做后台批量迁移；确需批量处理时按应用逐个走上述流程并逐个验证；
- 未经目标环境所有者确认，不修改远端工作区（本任务本轮未执行远端修改）；
- 生产 `[build]`（发布构建）命令不在本流程范围内，保持原策略。
