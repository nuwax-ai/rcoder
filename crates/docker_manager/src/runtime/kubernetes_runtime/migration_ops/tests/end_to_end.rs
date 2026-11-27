use super::*;
use std::path::{Path, PathBuf};
use std::process::Command;

const STUB_LOG: &str = "RCODER_STUB_RCLONE_LOG";
const STUB_EXIT: &str = "RCODER_STUB_RCLONE_EXIT";
/// 桩 rclone 每次被调用先打这一行, 用它在日志里切开各次调用。
const STUB_CALL: &str = "==rclone-call==";
const TOOL: &str = ".local/share/pnpm/.tools/pnpm/10.18.2_tmp_1_0";

static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    bin: PathBuf,
    src: PathBuf,
    dst: PathBuf,
    log: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.root));
    }
}

impl Fixture {
    fn new() -> Option<Self> {
        let probe = Command::new("bash")
            .arg("--version")
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !probe.status.success() {
            panic!("bash --version 失败");
        }
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("rcoder-backfill-job-{}-{id}", std::process::id()));
        drop(std::fs::remove_dir_all(&root));
        std::fs::create_dir_all(root.join("bin")).ok()?;
        // 假 rclone：每次调用先打分隔行, 再把 argv 逐行打进日志文件; copy 按给定的退出码退出
        let stub = root.join("bin/rclone");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\n\
                 printf '%s\\n' '{STUB_CALL}' >>\"${{{STUB_LOG}}}\"\n\
                 printf '%s\\n' \"$@\" >>\"${{{STUB_LOG}}}\"\n\
                 if [ \"$1\" = version ]; then echo 'rclone v0'; exit 0; fi\n\
                 exit \"${{{STUB_EXIT}:-0}}\"\n"
            ),
        )
        .ok()?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).ok()?;
        }
        Some(Self {
            bin: root.join("bin"),
            src: root.join("src/u_1"),
            dst: root.join("dst"),
            log: root.join("rclone.log"),
            root,
        })
    }

    fn file(&self, tree: &Path, relative: &str) {
        let path = tree.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    fn link(&self, tree: &Path, relative: &str, target: &str) {
        let path = tree.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, path).unwrap();
    }

    /// pnpm 版本切换目录：源树完好；目标树少了 node_modules（旧排除规则迁出来的形态）。
    fn pnpm_trees(&self) {
        self.file(&self.src, &format!("{TOOL}/package.json"));
        self.file(&self.src, &format!("{TOOL}/node_modules/pnpm/bin/pnpm.cjs"));
        self.link(
            &self.src,
            &format!("{TOOL}/bin/pnpm"),
            "../node_modules/pnpm/bin/pnpm.cjs",
        );
        self.file(&self.dst, &format!("{TOOL}/package.json"));
        self.link(
            &self.dst,
            &format!("{TOOL}/bin/pnpm"),
            "../node_modules/pnpm/bin/pnpm.cjs",
        );
    }

    /// 跑 Job 脚本：返回 (退出码, 输出, 桩 rclone 各次调用收到的 argv)。
    fn run(&self, plan: &BackfillJobPlan, rclone_exit: i32) -> (i32, String, Vec<Vec<String>>) {
        drop(std::fs::remove_file(&self.log));
        let output = Command::new("bash")
            .arg("-c")
            .arg(backfill_job_script(plan))
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env(STUB_LOG, &self.log)
            .env(STUB_EXIT, rclone_exit.to_string())
            .output()
            .unwrap();
        let calls: Vec<Vec<String>> = std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .split(&format!("{STUB_CALL}\n"))
            .skip(1)
            .map(|call| call.lines().map(str::to_string).collect())
            .collect();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string()
                + &String::from_utf8_lossy(&output.stderr),
            calls,
        )
    }

    /// 共享卷上、用户目录旁边的东西（例：顶层的 `.agent-store`）。
    fn shared(&self, relative: &str) -> PathBuf {
        let path = self.root.join("src").join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "x").unwrap();
        path
    }

    fn physical(&self, path: &Path) -> String {
        path.canonicalize().unwrap().to_string_lossy().into_owned()
    }
}

fn plan_at(fixture: &Fixture, paths: &[&str], only: &[&str]) -> BackfillJobPlan {
    let mut plan = backfill_plan();
    plan.src_dir = fixture.src.to_string_lossy().into_owned();
    plan.dst_dir = fixture.dst.to_string_lossy().into_owned();
    plan.backfill = shared_types::WorkspaceBackfill {
        paths: paths.iter().map(|p| (*p).to_string()).collect(),
        only: only.iter().map(|p| (*p).to_string()).collect(),
    };
    plan
}

fn copy_call(calls: &[Vec<String>]) -> &[String] {
    calls
        .iter()
        .find(|call| call.first().is_some_and(|first| first == "copy"))
        .unwrap_or_else(|| panic!("rclone copy 没被调用: {calls:?}"))
}

/// 补迁脚本给每次拷贝带上限（目标卷可用空间减余量, 数值随机器变）：核对 argv 时摘掉它。
fn without_budget(mut call: Vec<String>) -> Vec<String> {
    let index = call.iter().position(|arg| arg == "--max-transfer").unwrap();
    assert!(call[index + 1].parse::<u64>().unwrap() > 0, "上限是字节数");
    call.drain(index..index + 2);
    call
}

fn rules_of(call: &[String]) -> Vec<String> {
    (0..call.len().saturating_sub(1))
        .filter(|&index| call[index] == "--filter")
        .map(|index| call[index + 1].clone())
        .collect()
}

#[test]
fn job_script_runs_backfill_then_verifies_the_copied_scope() {
    let Some(fixture) = Fixture::new() else {
        eprintln!("skipping: bash not found");
        return;
    };
    fixture.pnpm_trees();
    let (code, stdout, calls) = fixture.run(&plan_at(&fixture, &[".local"], &[]), 0);
    assert_eq!(code, 1, "{stdout}");
    // 参数一层层传到底, 每个值恰好一个 argv；过滤规则在最前, 范围的放行在后
    assert_eq!(
        without_budget(copy_call(&calls).to_vec()),
        [
            "copy",
            &fixture.physical(&fixture.src),
            &fixture.physical(&fixture.dst),
            "--links",
            "--metadata",
            "--ignore-existing",
            "--transfers",
            "8",
            "--checkers",
            "8",
            "--stats-one-line",
            "--stats",
            "60s",
            "--filter",
            "- .cache/",
            "--filter",
            "- .npm/_npx/",
            // 不带 `--only`：范围之下整个都要，源上的空目录也建
            "--create-empty-src-dirs",
            "--filter",
            "+ .local/**",
            "--filter",
            "- *",
        ]
    );
    // 桩没有真的拷: 目标卷上 pnpm 的软链还悬空——校验范围正是刚拷过的 .local, 报出来, Job 失败
    assert!(
        stdout.contains(&format!(
            "[migrate] verify links under: .local\n[verify] 软链坏了: {TOOL}/bin/pnpm"
        )),
        "{stdout}"
    );
    // `rclone version` 一次（Job 开头）+ 拷贝一次；校验不再碰 rclone
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(parse_backfill_report(&stdout).copied, [".local"]);
}

/// 补完之后软链是通的：Job 成功, 校验只看刚拷过的范围。
#[test]
fn job_succeeds_when_the_links_under_the_copied_scope_resolve() {
    let Some(fixture) = Fixture::new() else {
        eprintln!("skipping: bash not found");
        return;
    };
    fixture.pnpm_trees();
    fixture.file(
        &fixture.dst,
        &format!("{TOOL}/node_modules/pnpm/bin/pnpm.cjs"),
    );
    // 别处的旧账不算在这次补迁头上: proj 里有坏软链, 但这次没补它
    fixture.file(&fixture.src, "proj/node_modules/dep/index.js");
    fixture.link(&fixture.src, "proj/bin/dev", "../node_modules/.bin/vite");
    fixture.link(&fixture.dst, "proj/bin/dev", "../node_modules/.bin/vite");
    let (code, stdout, _) = fixture.run(&plan_at(&fixture, &[".local"], &[]), 0);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains(
            "[migrate] verify links under: .local\n[verify] 软链完整性：核对软链 1 条，损坏 0 处"
        ),
        "{stdout}"
    );
}

/// 补迁脚本自己的退出码只有三种（0 完成、1 没拷成功或空间不够、2 参数不对）, rclone 的
/// 退出码在它的输出里。Job 以补迁脚本的退出码结束。
#[test]
fn a_failed_copy_fails_the_job_before_any_verification() {
    let Some(fixture) = Fixture::new() else {
        eprintln!("skipping: bash not found");
        return;
    };
    fixture.pnpm_trees();
    let (code, stdout, _) = fixture.run(&plan_at(&fixture, &[".local"], &[]), 3);
    assert_eq!(code, 1, "{stdout}");
    assert!(
        stdout.contains("[migrate] backfill did not finish (exit 1)"),
        "{stdout}"
    );
    assert!(stdout.contains("没拷成功（rclone 退出码 3）"), "{stdout}");
    assert!(
        !stdout.contains("verify links under"),
        "没拷完就不该往下校验"
    );
}

/// 目标卷空间不够：rclone 以 8 退出, 补迁脚本把它报成自己的 1（带扩容提示）, Job 跟着失败。
#[test]
fn running_out_of_space_fails_the_job() {
    let Some(fixture) = Fixture::new() else {
        eprintln!("skipping: bash not found");
        return;
    };
    fixture.pnpm_trees();
    let (code, stdout, _) = fixture.run(&plan_at(&fixture, &[".local"], &[]), 8);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("目标卷剩余空间不够"), "{stdout}");
    assert!(stdout.contains("扩容后重跑"), "{stdout}");
}

/// 源上没有的范围被跳过、报成 skipped；一个范围都没拷就不起校验。
#[test]
fn nothing_copied_means_nothing_verified() {
    let Some(fixture) = Fixture::new() else {
        eprintln!("skipping: bash not found");
        return;
    };
    std::fs::create_dir_all(&fixture.dst).unwrap();
    std::fs::create_dir_all(&fixture.src).unwrap();
    let (code, stdout, calls) = fixture.run(&plan_at(&fixture, &[".nope"], &[]), 0);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("[backfill] skipped: .nope"), "{stdout}");
    assert!(!stdout.contains("verify links under"), "{stdout}");
    assert_eq!(calls.len(), 1, "只有开头的 rclone version: {calls:?}");
    let empty = Vec::<String>::new();
    let report = parse_backfill_report(&stdout);
    assert_eq!(report.skipped, [".nope"]);
    assert_eq!(report.copied, empty);
}

/// 实测踩过（.19 批量补迁把用户 1 误判成失败）：共享卷上用户目录的旁边还有别的东西
/// （顶层的 .agent-store）, 项目里 `../../../.agent-store/…` 在源上能解析——但那是退到了
/// 用户目录之外。用户容器里根就是用户目录, 这种软链从来就不通, 不是迁移弄坏的。
#[test]
fn a_link_that_escapes_the_home_on_the_source_is_not_migration_damage() {
    let Some(fixture) = Fixture::new() else {
        eprintln!("skipping: bash not found");
        return;
    };
    fixture.shared(".agent-store/3994/skills/s.md");
    fixture.file(&fixture.src, "proj/package.json");
    fixture.link(
        &fixture.src,
        "proj/.agents/skills",
        "../../../.agent-store/3994/skills",
    );
    fixture.file(&fixture.dst, "proj/package.json");
    // 旧规则迁出来的卷上这条软链在（桩 rclone 不会拷）
    fixture.link(
        &fixture.dst,
        "proj/.agents/skills",
        "../../../.agent-store/3994/skills",
    );
    assert!(
        fixture.src.join("proj/.agents/skills").exists(),
        "场景没造对: 源上应该是通的"
    );
    assert!(
        !fixture.dst.join("proj/.agents/skills").exists(),
        "场景没造对: 目标上应该悬空"
    );
    let (code, stdout, _) = fixture.run(&plan_at(&fixture, &["*"], &[]), 0);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains("[verify] 软链完整性：核对软链 1 条，损坏 0 处"),
        "照常核对, 只是不算损坏: {stdout}"
    );
}

/// 只补项目依赖的用法（修旧排除规则留下的坏卷）：`*` + `--only`。每类要给两条规则
/// （`范围/模式` 管范围根下的, `范围/**/模式` 管更深层的）；带空格、引号的路径与
/// 模式原样是一个参数。
#[test]
fn only_patterns_and_odd_paths_reach_rclone_as_single_arguments() {
    let Some(fixture) = Fixture::new() else {
        eprintln!("skipping: bash not found");
        return;
    };
    fixture.file(&fixture.src, "proj-a/node_modules/dep/index.js");
    fixture.file(&fixture.src, "proj-a/package.json");
    fixture.file(&fixture.dst, "proj-a/package.json");
    // 切换后用户删掉的项目: 目标上没有, `*` 不补它
    fixture.file(&fixture.src, "proj-gone/node_modules/dep/index.js");
    fixture.file(&fixture.src, "it's proj/node_modules/dep/index.js");
    let (code, stdout, calls) = fixture.run(
        &plan_at(&fixture, &["*", "it's proj"], &["node_modules/**"]),
        0,
    );
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(calls.len(), 3, "{calls:?}");
    // 带 `--only` 时不带"建空目录"的参数（rclone 没有对应的 `--no-` 写法，写了会以 2 退出）：
    // 拿真实 rclone 核对时发现它会把源上的目录骨架在目标卷上建出来（旧规则排除的 dist/、
    // 切换后删掉的子目录以空目录的形式回来），与"只补匹配的条目"不符。不带 `--only` 的那次
    // 拷贝参数是逐项钉死的（上一个用例），里面有它。
    for call in &calls[1..] {
        assert!(
            !call
                .iter()
                .any(|arg| arg.starts_with("--create-empty-src-dirs")),
            "{call:?}"
        );
    }
    assert_eq!(
        rules_of(&calls[1]),
        [
            "- .cache/",
            "- .npm/_npx/",
            "+ proj-a/node_modules/**",
            "+ proj-a/**/node_modules/**",
            "- *",
        ]
    );
    assert_eq!(
        rules_of(&calls[2]),
        [
            "- .cache/",
            "- .npm/_npx/",
            "+ it's proj/node_modules/**",
            "+ it's proj/**/node_modules/**",
            "- *",
        ]
    );
    assert_eq!(
        parse_backfill_report(&stdout).copied,
        ["proj-a", "it's proj"]
    );
    assert!(!stdout.contains("proj-gone"), "{stdout}");
}
