//! git 套件场景与 fixture 准备。

use std::fs;
use std::path::Path;
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
use reqwest::Method;
use serde_json::{Value, json};

use crate::scenarios::*;
use crate::types::*;

pub(crate) fn git_scenario(
    name: impl Into<String>,
    project_id: &str,
    spec: RequestSpec,
    preparation: Option<GitPreparation>,
) -> GitScenario {
    GitScenario {
        name: name.into(),
        project_id: project_id.to_string(),
        spec,
        preparation,
    }
}

pub(crate) fn git_get(project_id: &str, route: &str) -> RequestSpec {
    get_spec(format!(
        "/api/git/{route}?workspaceType=pageApp&projectId={project_id}"
    ))
}

pub(crate) fn git_json(
    project_id: &str,
    method: Method,
    path: &str,
    mut body: Value,
) -> Result<RequestSpec> {
    let object = body
        .as_object_mut()
        .context("Git A/B request body must be an object")?;
    object.insert("workspaceType".to_string(), json!("pageApp"));
    object.insert("projectId".to_string(), json!(project_id));
    json_spec(method, path, body)
}

pub(crate) fn append_git_seed(
    scenarios: &mut Vec<GitScenario>,
    project_id: &str,
    prefix: &str,
    second_commit: bool,
) -> Result<()> {
    scenarios.push(git_scenario(
        format!("{prefix}-init"),
        project_id,
        git_json(project_id, Method::POST, "/api/git/init", json!({}))?,
        None,
    ));
    scenarios.push(git_scenario(
        format!("{prefix}-add-initial"),
        project_id,
        git_json(
            project_id,
            Method::POST,
            "/api/git/add",
            json!({"files":["README.md","src/index.html"]}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        format!("{prefix}-commit-initial"),
        project_id,
        git_json(
            project_id,
            Method::POST,
            "/api/git/commit",
            json!({
                "message":"A/B initial commit",
                "authorName":"File Server A-B",
                "authorEmail":"ab@example.invalid"
            }),
        )?,
        None,
    ));
    if second_commit {
        scenarios.push(git_scenario(
            format!("{prefix}-add-second"),
            project_id,
            git_json(
                project_id,
                Method::POST,
                "/api/git/add",
                json!({"files":["README.md","src/new.txt"]}),
            )?,
            Some(GitPreparation::SecondCommitChange),
        ));
        scenarios.push(git_scenario(
            format!("{prefix}-commit-second"),
            project_id,
            git_json(
                project_id,
                Method::POST,
                "/api/git/commit",
                json!({
                    "message":"A/B second commit",
                    "authorName":"File Server A-B",
                    "authorEmail":"ab@example.invalid"
                }),
            )?,
            None,
        ));
    }
    Ok(())
}

pub(crate) fn git_scenarios() -> Result<Vec<GitScenario>> {
    let main = GIT_PROJECT_MAIN;
    let mut scenarios = vec![
        git_scenario(
            "git-init",
            main,
            git_json(main, Method::POST, "/api/git/init", json!({}))?,
            None,
        ),
        git_scenario(
            "git-status-before-first-commit",
            main,
            git_get(main, "status"),
            None,
        ),
        git_scenario(
            "git-add-initial-files",
            main,
            git_json(
                main,
                Method::POST,
                "/api/git/add",
                json!({"files":["README.md","src/index.html"]}),
            )?,
            None,
        ),
        git_scenario(
            "git-commit-initial",
            main,
            git_json(
                main,
                Method::POST,
                "/api/git/commit",
                json!({
                    "message":"A/B initial commit",
                    "authorName":"File Server A-B",
                    "authorEmail":"ab@example.invalid"
                }),
            )?,
            None,
        ),
        git_scenario("git-status-clean", main, git_get(main, "status"), None),
        git_scenario(
            "git-read-head-file",
            main,
            git_json(
                main,
                Method::POST,
                "/api/git/file-content",
                json!({"filePath":"src/index.html","ref":"HEAD"}),
            )?,
            None,
        ),
        // TS 02bec84 (v1.5.5): 目录在两个分支都是 VALIDATION 400（message/details
        // 逐字对比），而非树清单文本（旧 git show）或 500。
        git_scenario(
            "git-file-content-directory-ref",
            main,
            {
                let mut spec = git_json(
                    main,
                    Method::POST,
                    "/api/git/file-content",
                    json!({"filePath":"src","ref":"HEAD"}),
                )?;
                spec.expected_status = ExpectedStatus::ClientError4xx;
                spec.normalized_paths = vec!["/error/requestId".into(), "/error/timestamp".into()];
                spec
            },
            None,
        ),
        git_scenario(
            "git-file-content-directory-worktree",
            main,
            {
                let mut spec = git_json(
                    main,
                    Method::POST,
                    "/api/git/file-content",
                    json!({"filePath":"src","ref":"worktree"}),
                )?;
                spec.expected_status = ExpectedStatus::ClientError4xx;
                spec.normalized_paths = vec!["/error/requestId".into(), "/error/timestamp".into()];
                spec
            },
            None,
        ),
    ];

    let branch_list = git_get(main, "branches");
    scenarios.push(git_scenario(
        "git-create-branch",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/branch-create",
            json!({"branchName":"ab-review"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-list-branches-after-create",
        main,
        branch_list,
        None,
    ));
    scenarios.push(git_scenario(
        "git-switch-main",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/branch-switch",
            json!({"branchName":"main"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-create-tag",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/tag-create",
            json!({"tagName":"ab-v1","message":"A/B baseline tag"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-list-tags",
        main,
        git_get(main, "tags"),
        None,
    ));
    scenarios.push(git_scenario(
        "git-delete-tag",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/tag-delete",
            json!({"tagName":"ab-v1"}),
        )?,
        None,
    ));

    scenarios.push(git_scenario(
        "git-status-after-worktree-change",
        main,
        git_get(main, "status"),
        Some(GitPreparation::MainWorktreeChange),
    ));
    scenarios.push(git_scenario(
        "git-worktree-diff",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/diff",
            json!({"source":"worktree"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-add-second-change",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/add",
            json!({"files":["README.md","src/new.txt"]}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-staged-diff",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/diff",
            json!({"source":"staged"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-unstage-new-file",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/unstage",
            json!({"files":["src/new.txt"]}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-status-after-unstage",
        main,
        git_get(main, "status"),
        None,
    ));
    scenarios.push(git_scenario(
        "git-restage-second-change",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/add",
            json!({"files":["README.md","src/new.txt"]}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-commit-second",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/commit",
            json!({
                "message":"A/B second commit",
                "authorName":"File Server A-B",
                "authorEmail":"ab@example.invalid"
            }),
        )?,
        None,
    ));
    let mut git_log = get_spec(format!(
        "/api/git/log?workspaceType=pageApp&projectId={main}&maxCount=10"
    ));
    git_log.normalized_paths = vec![
        "/commits/0/hash".into(),
        "/commits/0/date".into(),
        "/commits/1/hash".into(),
        "/commits/1/date".into(),
    ];
    scenarios.push(git_scenario("git-log", main, git_log, None));

    let branch_delete = GIT_PROJECT_BRANCH_DELETE;
    append_git_seed(
        &mut scenarios,
        branch_delete,
        "git-branch-delete-seed",
        false,
    )?;
    scenarios.push(git_scenario(
        "git-delete-branch",
        branch_delete,
        git_json(
            branch_delete,
            Method::POST,
            "/api/git/branch-delete",
            json!({"branchName":"ab-review"}),
        )?,
        Some(GitPreparation::CreateDeleteBranch),
    ));
    scenarios.push(git_scenario(
        "git-list-branches-after-delete",
        branch_delete,
        git_get(branch_delete, "branches"),
        None,
    ));

    let revert = GIT_PROJECT_REVERT;
    append_git_seed(&mut scenarios, revert, "git-revert-seed", true)?;
    let mut revert_spec = git_json(
        revert,
        Method::POST,
        "/api/git/revert",
        json!({
            "target":"HEAD~1",
            "message":"A/B revert to initial tree",
            "authorName":"File Server A-B",
            "authorEmail":"ab@example.invalid"
        }),
    )?;
    revert_spec.normalized_paths = vec!["/commit".into(), "/target".into(), "/previousHead".into()];
    scenarios.push(git_scenario(
        "git-revert-to-initial",
        revert,
        revert_spec,
        None,
    ));

    let reset_mixed = GIT_PROJECT_RESET_MIXED;
    append_git_seed(&mut scenarios, reset_mixed, "git-reset-mixed-seed", true)?;
    let mut reset_mixed_spec = git_json(
        reset_mixed,
        Method::POST,
        "/api/git/reset",
        json!({"target":"HEAD~1","mode":"mixed"}),
    )?;
    reset_mixed_spec.normalized_paths = vec!["/previousHead".into()];
    scenarios.push(git_scenario(
        "git-reset-mixed-to-first",
        reset_mixed,
        reset_mixed_spec,
        None,
    ));

    let reset_hard = GIT_PROJECT_RESET_HARD;
    append_git_seed(&mut scenarios, reset_hard, "git-reset-hard-seed", true)?;
    let mut reset_hard_spec = git_json(
        reset_hard,
        Method::POST,
        "/api/git/reset",
        json!({"target":"HEAD~1","mode":"hard"}),
    )?;
    reset_hard_spec.normalized_paths = vec!["/previousHead".into()];
    scenarios.push(git_scenario(
        "git-reset-hard-to-first",
        reset_hard,
        reset_hard_spec,
        Some(GitPreparation::ResetHardDirty),
    ));

    let reset_soft = GIT_PROJECT_RESET_SOFT;
    append_git_seed(&mut scenarios, reset_soft, "git-reset-soft-seed", true)?;
    let mut reset_soft_spec = git_json(
        reset_soft,
        Method::POST,
        "/api/git/reset",
        json!({"target":"HEAD~1","mode":"soft"}),
    )?;
    reset_soft_spec.normalized_paths = vec!["/previousHead".into()];
    scenarios.push(git_scenario(
        "git-reset-soft-to-first",
        reset_soft,
        reset_soft_spec,
        None,
    ));

    let checkout = GIT_PROJECT_CHECKOUT;
    append_git_seed(&mut scenarios, checkout, "git-checkout-seed", false)?;
    scenarios.push(git_scenario(
        "git-checkout-head",
        checkout,
        git_json(
            checkout,
            Method::POST,
            "/api/git/checkout",
            json!({"target":"HEAD"}),
        )?,
        Some(GitPreparation::CheckoutDirty),
    ));
    scenarios.push(git_scenario(
        "git-status-after-checkout",
        checkout,
        git_get(checkout, "status"),
        None,
    ));

    let discard = GIT_PROJECT_DISCARD;
    append_git_seed(&mut scenarios, discard, "git-discard-seed", false)?;
    scenarios.push(git_scenario(
        "git-discard-all",
        discard,
        git_json(
            discard,
            Method::POST,
            "/api/git/discard",
            json!({"files":[]}),
        )?,
        Some(GitPreparation::DiscardDirty),
    ));

    let merge_conflict = GIT_PROJECT_MERGE_CONFLICT;
    append_git_seed(
        &mut scenarios,
        merge_conflict,
        "git-merge-conflict-seed",
        false,
    )?;
    scenarios.push(git_scenario(
        "git-merge-conflict-create-feature",
        merge_conflict,
        git_json(
            merge_conflict,
            Method::POST,
            "/api/git/branch-create",
            json!({"branchName":"ab-conflict-feature"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-merge-conflict-switch-feature",
        merge_conflict,
        git_json(
            merge_conflict,
            Method::POST,
            "/api/git/branch-switch",
            json!({"branchName":"ab-conflict-feature"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-merge-conflict-stage-feature",
        merge_conflict,
        git_json(
            merge_conflict,
            Method::POST,
            "/api/git/add",
            json!({"files":["README.md"]}),
        )?,
        Some(GitPreparation::MergeConflictFeatureChange),
    ));
    scenarios.push(git_scenario(
        "git-merge-conflict-commit-feature",
        merge_conflict,
        git_json(
            merge_conflict,
            Method::POST,
            "/api/git/commit",
            json!({
                "message":"A/B conflict feature commit",
                "authorName":"File Server A-B",
                "authorEmail":"ab@example.invalid"
            }),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-merge-conflict-switch-main",
        merge_conflict,
        git_json(
            merge_conflict,
            Method::POST,
            "/api/git/branch-switch",
            json!({"branchName":"main"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-merge-conflict-stage-main",
        merge_conflict,
        git_json(
            merge_conflict,
            Method::POST,
            "/api/git/add",
            json!({"files":["README.md"]}),
        )?,
        Some(GitPreparation::MergeConflictMainChange),
    ));
    scenarios.push(git_scenario(
        "git-merge-conflict-commit-main",
        merge_conflict,
        git_json(
            merge_conflict,
            Method::POST,
            "/api/git/commit",
            json!({
                "message":"A/B conflict main commit",
                "authorName":"File Server A-B",
                "authorEmail":"ab@example.invalid"
            }),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-status-after-merge-conflict",
        merge_conflict,
        git_get(merge_conflict, "status"),
        Some(GitPreparation::CreateMergeConflict),
    ));

    Ok(scenarios)
}

pub(crate) fn prepare_git_fixtures(root: &Path) -> Result<()> {
    for project_id in GIT_PROJECT_IDS {
        let project = root.join("project-workspace").join(project_id);
        fs::create_dir_all(project.join("src"))
            .with_context(|| format!("create Git fixture directory {}", project.display()))?;
        fs::write(project.join("README.md"), "Git A/B fixture\n")
            .with_context(|| format!("write Git fixture README in {}", project.display()))?;
        fs::write(project.join("src/index.html"), "<main>Git fixture</main>\n")
            .with_context(|| format!("write Git fixture entry in {}", project.display()))?;
    }
    Ok(())
}

pub(crate) fn prepare_git_scenario(
    preparation: GitPreparation,
    project_id: &str,
    rust_root: &Path,
    ts_root: &Path,
) -> Result<()> {
    for root in [rust_root, ts_root] {
        let project = root.join("project-workspace").join(project_id);
        match preparation {
            GitPreparation::MainWorktreeChange | GitPreparation::SecondCommitChange => {
                fs::write(project.join("README.md"), "Git A/B fixture changed\n")?;
                fs::write(project.join("src/new.txt"), "new staged fixture\n")?;
            }
            GitPreparation::CheckoutDirty => {
                fs::write(project.join("README.md"), "dirty before checkout\n")?;
                fs::write(
                    project.join("checkout-extra.txt"),
                    "checkout keeps unrelated file\n",
                )?;
            }
            GitPreparation::DiscardDirty => {
                fs::write(project.join("README.md"), "dirty before discard\n")?;
                fs::write(
                    project.join("discard-extra.txt"),
                    "discard removes untracked file\n",
                )?;
            }
            GitPreparation::ResetHardDirty => {
                fs::write(project.join("README.md"), "dirty before hard reset\n")?;
                fs::write(
                    project.join("reset-extra.txt"),
                    "hard reset removes untracked file\n",
                )?;
            }
            GitPreparation::CreateDeleteBranch => {
                run_git_fixture_command(&project, &["branch", "ab-review"])?;
            }
            GitPreparation::MergeConflictFeatureChange => {
                fs::write(project.join("README.md"), "feature branch version\n")?;
            }
            GitPreparation::MergeConflictMainChange => {
                fs::write(project.join("README.md"), "main branch version\n")?;
            }
            GitPreparation::CreateMergeConflict => {
                run_git_merge_conflict_fixture(&project, "ab-conflict-feature", "README.md")?;
            }
        }
    }
    Ok(())
}

pub(crate) fn run_git_merge_conflict_fixture(
    project: &Path,
    branch: &str,
    path: &str,
) -> Result<()> {
    let merge = ProcessCommand::new("git")
        .arg("-C")
        .arg(project)
        .args([
            "-c",
            "user.name=File Server A-B",
            "-c",
            "user.email=ab@example.invalid",
            "merge",
            "--no-commit",
            "--no-ff",
            branch,
        ])
        .output()
        .with_context(|| format!("start Git merge fixture in {}", project.display()))?;
    if merge.status.code() != Some(1) {
        bail!(
            "expected a merge conflict for {path} in {} but git merge exited {}: {}",
            project.display(),
            merge.status,
            String::from_utf8_lossy(&merge.stderr).trim()
        );
    }

    let unmerged = ProcessCommand::new("git")
        .arg("-C")
        .arg(project)
        .args(["ls-files", "-u", "--", path])
        .output()
        .with_context(|| format!("inspect unmerged Git index in {}", project.display()))?;
    if !unmerged.status.success()
        || !String::from_utf8_lossy(&unmerged.stdout)
            .lines()
            .any(|line| line.ends_with(path))
    {
        bail!(
            "Git merge returned conflict status but did not leave {path} unmerged in {}: {}",
            project.display(),
            String::from_utf8_lossy(&unmerged.stderr).trim()
        );
    }
    Ok(())
}

pub(crate) fn run_git_fixture_command(project: &Path, args: &[&str]) -> Result<()> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .output()
        .with_context(|| {
            format!(
                "start git {} for fixture {}",
                args.join(" "),
                project.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "git {} failed for fixture {} with {}: {}",
            args.join(" "),
            project.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
