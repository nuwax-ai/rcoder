use super::*;
use crate::service::computer_ws::{create_workspace, helpers::now_nanos};

fn seed_locked_skill(workspace: &Path, name: &str, marker: &str) {
    let dir = workspace.join(".agents").join("skills").join(name);
    std::fs::create_dir_all(&dir).expect("skill dir");
    std::fs::write(dir.join(DYNAMIC_ADD_LOCK), b"").expect("lock");
    std::fs::write(dir.join("SKILL.md"), format!("# {marker}")).expect("content");
}

#[test]
fn preserve_receipt_atomic_write_failure_keeps_previous_receipt() {
    use std::io::Write as _;
    let root = tempfile::tempdir().expect("root");
    let path = root.path().join("receipt.json");
    std::fs::write(&path, b"previous-complete-receipt").unwrap();
    let result = publish_preserve_receipt(&path, |file| {
        file.write_all(b"partial-new-receipt")?;
        Err(std::io::Error::other("injected receipt writer failure"))
    });
    assert!(result.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"previous-complete-receipt");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn preserve_receipt_rejects_invalid_names_before_any_recovery_mutation() {
    let root = tempfile::tempdir().expect("root");
    let outside = root.path().join("outside");
    let invalid_names = [
        outside.to_string_lossy().into_owned(),
        "../outside".to_string(),
        "good/../outside".to_string(),
        "nested/skill".to_string(),
        "skill/".to_string(),
        String::new(),
        ".".to_string(),
        "..".to_string(),
    ];
    for (index, invalid_name) in invalid_names.into_iter().enumerate() {
        let workspace = root.path().join(format!("ws-{index}"));
        let area = preserve_area(&workspace);
        fs::create_dir_all(area.join("good")).await.unwrap();
        fs::write(area.join("good/SKILL.md"), b"only-good-copy")
            .await
            .unwrap();
        fs::create_dir_all(&outside).await.unwrap();
        fs::write(outside.join("sentinel"), b"outside-original")
            .await
            .unwrap();
        let receipt = PreserveReceipt {
            version: PRESERVE_RECEIPT_VERSION,
            operation_id: "invalid-name-fixture".into(),
            workspace_root: fs::canonicalize(&workspace)
                .await
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            skills: vec!["good".into(), invalid_name.clone()],
        };
        // Persist untrusted input directly: the normal writer must reject it.
        let original_receipt = serde_json::to_vec(&receipt).unwrap();
        fs::write(preserve_receipt_path(&workspace), &original_receipt)
            .await
            .unwrap();

        let result = resume_unfinished_preservation(&workspace).await;
        assert!(
            outside.join("sentinel").is_file(),
            "invalid receipt name {invalid_name:?} must not delete outside data"
        );
        assert_eq!(
            fs::read(outside.join("sentinel")).await.unwrap(),
            b"outside-original"
        );
        assert!(result.is_err(), "invalid name {invalid_name:?} must fail");
        assert_eq!(
            fs::read(area.join("good/SKILL.md")).await.unwrap(),
            b"only-good-copy",
            "the whole list must be validated before moving its first valid item"
        );
        assert_eq!(
            fs::read(preserve_receipt_path(&workspace)).await.unwrap(),
            original_receipt,
            "the original receipt must remain available for inspection"
        );
    }
}

#[tokio::test]
async fn preserve_receipt_keeps_source_when_target_is_an_unproven_directory() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("ws");
    seed_locked_skill(&workspace, "skill-a", "original");
    let preserved = preserve_locked_skills(&workspace.join(".agents/skills"), &workspace)
        .await
        .unwrap();
    drop(preserved);
    let original_receipt = fs::read(preserve_receipt_path(&workspace)).await.unwrap();
    let target = workspace.join(".agents/skills/skill-a");
    fs::create_dir_all(&target).await.unwrap();
    fs::write(target.join("SKILL.md"), b"foreign-directory")
        .await
        .unwrap();

    let result = resume_unfinished_preservation(&workspace).await;
    assert!(
        result.is_err(),
        "an arbitrary directory is not a restore receipt"
    );
    assert_eq!(
        fs::read(preserved_skill_path(&workspace, "skill-a").join("SKILL.md"))
            .await
            .unwrap(),
        b"# original",
        "the only original copy must not be deleted"
    );
    assert_eq!(
        fs::read(target.join("SKILL.md")).await.unwrap(),
        b"foreign-directory"
    );
    assert_eq!(
        fs::read(preserve_receipt_path(&workspace)).await.unwrap(),
        original_receipt
    );
}

#[tokio::test]
async fn preserve_receipt_keeps_evidence_when_both_skill_copies_are_missing() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("ws");
    seed_locked_skill(&workspace, "skill-a", "original");
    let preserved = preserve_locked_skills(&workspace.join(".agents/skills"), &workspace)
        .await
        .unwrap();
    drop(preserved);
    let original_receipt = fs::read(preserve_receipt_path(&workspace)).await.unwrap();
    fs::remove_dir_all(preserved_skill_path(&workspace, "skill-a"))
        .await
        .unwrap();

    let result = resume_unfinished_preservation(&workspace).await;
    assert!(
        result.is_err(),
        "missing both copies cannot mean recovery succeeded"
    );
    assert_eq!(
        fs::read(preserve_receipt_path(&workspace)).await.unwrap(),
        original_receipt,
        "missing evidence must not be discarded"
    );
    assert!(preserve_area(&workspace).is_dir());
}

#[tokio::test]
async fn preserve_receipt_keeps_truncated_input_and_preserved_skill() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("ws");
    seed_locked_skill(&workspace, "skill-a", "original");
    let preserved = preserve_locked_skills(&workspace.join(".agents/skills"), &workspace)
        .await
        .unwrap();
    drop(preserved);
    let preserved_path = preserved_skill_path(&workspace, "skill-a");
    let truncated = br#"{"version":1,"skills":["skill-a""#;
    fs::write(preserve_receipt_path(&workspace), truncated)
        .await
        .unwrap();

    let error = resume_unfinished_preservation(&workspace)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("invalid preserve receipt"));
    assert_eq!(
        fs::read(preserve_receipt_path(&workspace)).await.unwrap(),
        truncated
    );
    assert_eq!(
        fs::read(preserved_path.join("SKILL.md")).await.unwrap(),
        b"# original"
    );
}

/// P1-2 反例(中断恢复闭环): preserve 完成、restore 被取消（模拟进程中断/
/// 请求取消）后, 原入口重试 `create_workspace` 必须凭持久回执恢复**全部**
/// 技能。修复前的实现入口只扫描当前 skills/, 重试返回成功而唯一副本留在
/// 不可达的随机目录（Codex remaining-data-protection.md §1 源码分析）。
#[tokio::test]
async fn interrupted_preservation_resumes_on_reentry() {
    let parent = tempfile::tempdir().expect("parent");
    let workspace = parent.path().join("ws");
    seed_locked_skill(&workspace, "skill-a", "a-original");
    seed_locked_skill(&workspace, "skill-b", "b-original");

    // preserve 完成（回执+两份副本就位）, 然后模拟中断: 不再 restore。
    let preserved = preserve_locked_skills(&workspace.join(".agents").join("skills"), &workspace)
        .await
        .expect("preserve");
    assert_eq!(preserved.1, ["skill-a", "skill-b"]);
    assert!(
        preserve_receipt_path(&workspace).is_file(),
        "receipt must be durable before any move"
    );
    assert!(
        !workspace
            .join(".agents")
            .join("skills")
            .join("skill-a")
            .exists()
    );
    drop(preserved); // guard drop 不删除（FS-05 语义保持）

    // 重试入口（等价于新进程对同一工作区调用 create_workspace 的前置步骤）
    resume_unfinished_preservation(&workspace)
        .await
        .expect("resume must complete the interrupted preservation");

    let skills = workspace.join(".agents").join("skills");
    for (name, marker) in [("skill-a", "a-original"), ("skill-b", "b-original")] {
        let restored = skills.join(name);
        assert!(
            restored.join(DYNAMIC_ADD_LOCK).is_file(),
            "{name} lock restored"
        );
        assert_eq!(
            fs::read_to_string(restored.join("SKILL.md")).await.unwrap(),
            format!("# {marker}"),
            "{name} content restored losslessly"
        );
    }
    assert!(
        !preserve_receipt_path(&workspace).exists(),
        "confirmed receipt must be cleaned"
    );
    assert!(
        !preserve_area(&workspace).exists(),
        "confirmed area must be cleaned"
    );
}

/// P1-2 反例(部分恢复幂等续行): skill-a 已恢复、skill-b 因目标被占失败 →
/// 清除冲突后原入口重试: 全部恢复, 且已恢复的 skill-a 内容**不被覆盖**。
#[tokio::test]
async fn partial_restore_resumes_idempotently_without_overwrite() {
    let parent = tempfile::tempdir().expect("parent");
    let workspace = parent.path().join("ws");
    seed_locked_skill(&workspace, "skill-a", "a-original");
    seed_locked_skill(&workspace, "skill-b", "b-original");
    let preserved = preserve_locked_skills(&workspace.join(".agents").join("skills"), &workspace)
        .await
        .expect("preserve");

    // 第一次 restore: skill-a 成功; skill-b 目标被普通文件占用 → 失败。
    fs::write(
        workspace.join(".agents").join("skills").join("skill-b"),
        b"placeholder",
    )
    .await
    .expect("placeholder");
    assert!(
        restore_locked_skills(&preserved, &workspace).await.is_err(),
        "occupied target must fail the first restore"
    );
    // skill-a 已回到权威位; 失败后回执与 skill-b 副本必须保留。
    assert!(preserve_receipt_path(&workspace).is_file());
    assert!(
        preserved_skill_path(&workspace, "skill-b")
            .join("SKILL.md")
            .is_file()
    );

    // 清除冲突 → 原入口重试: skill-b 恢复, skill-a 保持第一次恢复的内容。
    fs::remove_file(workspace.join(".agents").join("skills").join("skill-b"))
        .await
        .expect("clear placeholder");
    resume_unfinished_preservation(&workspace)
        .await
        .expect("retry must finish the remaining skill");
    let skills = workspace.join(".agents").join("skills");
    assert_eq!(
        fs::read_to_string(skills.join("skill-a/SKILL.md"))
            .await
            .unwrap(),
        "# a-original",
        "already-restored skill must not be overwritten"
    );
    assert_eq!(
        fs::read_to_string(skills.join("skill-b/SKILL.md"))
            .await
            .unwrap(),
        "# b-original"
    );
    assert!(!preserve_receipt_path(&workspace).exists());
}

/// P1-2 反例(preserve 中途失败精确续行): 回执含两项, 第一项已移入保留区、
/// 第二项仍在原位（第二项 move 失败的中断态）。续行必须两项都落回权威位:
/// 保留区副本搬回 + 原位项视为已恢复, 不重复移动、不丢失。
#[tokio::test]
async fn failed_second_preserve_resumes_exactly() {
    let parent = tempfile::tempdir().expect("parent");
    let workspace = parent.path().join("ws");
    seed_locked_skill(&workspace, "skill-a", "a-original");
    seed_locked_skill(&workspace, "skill-b", "b-original");

    // 手工构造"第二项 preserve 失败"的持久中断态: 回执记录两项,
    // 仅 skill-a 实际移入保留区; skill-b 留在 skills/ 原位。
    let area = preserve_area(&workspace);
    fs::create_dir_all(&area).await.expect("area");
    let skills = workspace.join(".agents").join("skills");
    fs::rename(skills.join("skill-a"), area.join("skill-a"))
        .await
        .expect("move first");
    let canonical = fs::canonicalize(&workspace).await.unwrap();
    persist_receipt(
        &workspace,
        &PreserveReceipt {
            version: PRESERVE_RECEIPT_VERSION,
            operation_id: "op".into(),
            workspace_root: canonical.to_string_lossy().into_owned(),
            skills: vec!["skill-a".into(), "skill-b".into()],
        },
    )
    .await
    .expect("receipt");

    resume_unfinished_preservation(&workspace)
        .await
        .expect("resume from exact receipt");

    assert_eq!(
        fs::read_to_string(skills.join("skill-a/SKILL.md"))
            .await
            .unwrap(),
        "# a-original",
        "moved copy must come back from the preserve area"
    );
    assert_eq!(
        fs::read_to_string(skills.join("skill-b/SKILL.md"))
            .await
            .unwrap(),
        "# b-original",
        "in-place item must stay authoritative"
    );
    assert!(!preserve_receipt_path(&workspace).exists());
}

/// P1-2 反例(两请求竞争): 同一工作区并发 create_workspace 不得互相删除
/// 保留来源或重复宣称完成——进程内按工作区互斥串行化。
#[tokio::test]
async fn concurrent_reentry_keeps_single_copy_and_completes() {
    let parent = tempfile::tempdir().expect("parent");
    let workspace = parent.path().join("ws");
    seed_locked_skill(&workspace, "skill-a", "a-original");

    let (first, second) = tokio::join!(
        create_workspace(&workspace, None, Vec::new(), None, None),
        create_workspace(&workspace, None, Vec::new(), None, None),
    );
    first.expect("first create succeeds");
    second.expect("serialized second create succeeds");
    let skill = workspace.join(".agents").join("skills").join("skill-a");
    assert_eq!(
        fs::read_to_string(skill.join("SKILL.md")).await.unwrap(),
        "# a-original",
        "exactly one authoritative copy survives the race"
    );
    assert!(!preserve_receipt_path(&workspace).exists());
    assert!(!preserve_area(&workspace).exists());
}

#[tokio::test]
async fn create_workspace_writes_agents_skills() {
    let tmp = std::env::temp_dir().join(format!("fs_cw_{}", now_nanos()));
    let res = create_workspace(&tmp, None, Vec::new(), None, None)
        .await
        .unwrap();
    assert!(tmp.join(".agents").join("skills").is_dir());
    assert!(tmp.join(".agents").join("agents").is_dir());
    // 无 file → 早退 message
    assert!(res.message.contains("no uploaded file"));
    // syncAgents 镜像目录 (grok/pi 临时屏蔽, 不再创建)
    assert!(tmp.join(".claude").join("skills").is_dir());
    assert!(tmp.join(".opencode").join("skills").is_dir());
    assert!(tmp.join(".codex").join("skills").is_dir());
    assert!(!tmp.join(".grok").join("skills").exists());
    assert!(!tmp.join(".pi").join("skills").exists());
    // sync_agents 写版本 marker (启动 reconciler 据此 O(1) 判断是否需补 sync)
    assert!(tmp.join(".agents").join(".sync_version").is_file());
    drop(fs::remove_dir_all(&tmp).await);
}
