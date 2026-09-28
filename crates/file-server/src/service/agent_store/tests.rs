use super::*;

#[cfg(test)]
mod cases {
    use super::*;

    #[tokio::test]
    async fn install_skill_dir_copies_and_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let dest_dir = tmp.path().join("skills");

        // v1
        let src1 = tmp.path().join("src1");
        fs::create_dir_all(&src1).await.unwrap();
        fs::write(src1.join("SKILL.md"), "v1").await.unwrap();
        install_skill_dir(&src1, &dest_dir, "my-skill", false)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(dest_dir.join("my-skill").join("SKILL.md"))
                .await
                .unwrap(),
            "v1"
        );

        // 覆盖安装 v2 (源已被 rename 移走, 用新源)
        let src2 = tmp.path().join("src2");
        fs::create_dir_all(&src2).await.unwrap();
        fs::write(src2.join("SKILL.md"), "v2").await.unwrap();
        install_skill_dir(&src2, &dest_dir, "my-skill", false)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(dest_dir.join("my-skill").join("SKILL.md"))
                .await
                .unwrap(),
            "v2"
        );
    }

    #[tokio::test]
    async fn update_agents_dir_copies_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src_agents");
        let dest = tmp.path().join("agents");
        fs::create_dir_all(src.join("reviewer")).await.unwrap();
        fs::create_dir_all(src.join("coder")).await.unwrap();
        fs::write(src.join("reviewer/agent.md"), "r").await.unwrap();
        fs::write(src.join("coder/agent.md"), "c").await.unwrap();

        update_agents_dir(Some(&src), &dest).await.unwrap();

        assert!(dest.join("reviewer/agent.md").exists());
        assert!(dest.join("coder/agent.md").exists());
    }

    #[tokio::test]
    async fn update_agents_dir_overwrites_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("agents");
        // 旧内容
        fs::create_dir_all(dest.join("old-agent")).await.unwrap();
        fs::write(dest.join("old-agent/agent.md"), "old")
            .await
            .unwrap();

        // 新源
        let src = tmp.path().join("src_agents");
        fs::create_dir_all(src.join("new-agent")).await.unwrap();
        fs::write(src.join("new-agent/agent.md"), "new")
            .await
            .unwrap();

        update_agents_dir(Some(&src), &dest).await.unwrap();

        // 新的已写入
        assert_eq!(
            fs::read_to_string(dest.join("new-agent/agent.md"))
                .await
                .unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn update_agents_dir_no_source_creates_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("agents");
        update_agents_dir(None, &dest).await.unwrap();
        assert!(dest.is_dir());
    }

    #[tokio::test]
    async fn prune_removes_unlisted_non_dynamic() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        fs::create_dir_all(skills.join("keep-me")).await.unwrap();
        fs::create_dir_all(skills.join("delete-me")).await.unwrap();
        fs::create_dir_all(skills.join("dynamic-me")).await.unwrap();
        fs::write(skills.join("dynamic-me").join(DYNAMIC_ADD_LOCK), "123")
            .await
            .unwrap();
        // 杂散文件 (非目录) 也应被删除而不是让 prune 报错 (对齐 TS fs.rm force 语义)
        fs::write(skills.join("stray-file.md"), "x").await.unwrap();

        let (removed, kept_dynamic) = prune_agent_skills(&skills, &["keep-me".to_string()])
            .await
            .unwrap();

        assert!(removed.contains(&"delete-me".to_string()));
        assert!(removed.contains(&"stray-file.md".to_string()));
        assert!(!removed.contains(&"dynamic-me".to_string()));
        assert!(kept_dynamic.contains(&"dynamic-me".to_string()));
        assert!(skills.join("keep-me").exists());
        assert!(!skills.join("delete-me").exists());
        assert!(!skills.join("stray-file.md").exists());
        assert!(skills.join("dynamic-me").exists());
    }

    #[test]
    fn agent_skill_exists_checks_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("my-skill")).unwrap();
        assert!(agent_skill_exists(tmp.path(), "my-skill"));
        assert!(!agent_skill_exists(tmp.path(), "nope"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn force_dir_symlink_creates_relative_link() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("store").join("skills");
        fs::create_dir_all(&target).await.unwrap();
        fs::write(target.join("SKILL.md"), "content").await.unwrap();

        let link = tmp.path().join("ws").join(".agents").join("skills");

        force_dir_symlink(&link, &target).await.unwrap();

        assert!(link.is_symlink(), "link should be a symlink on unix");
        assert_eq!(
            fs::read_to_string(link.join("SKILL.md")).await.unwrap(),
            "content"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn link_workspace_creates_all_agent_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("session");

        let store_skills = tmp.path().join("store").join("skills");
        let store_agents = tmp.path().join("store").join("agents");
        fs::create_dir_all(&store_skills).await.unwrap();
        fs::create_dir_all(&store_agents).await.unwrap();
        fs::write(store_skills.join("a.md"), "a").await.unwrap();
        fs::write(store_agents.join("b.md"), "b").await.unwrap();

        link_workspace_to_agent_store(&workspace, &store_skills, &store_agents)
            .await
            .unwrap();

        for dir in crate::service::skills::ALL_AGENT_DIRS {
            let s = workspace.join(dir).join("skills");
            let a = workspace.join(dir).join("agents");
            assert!(s.is_symlink(), "{dir}/skills should be symlink");
            assert!(a.is_symlink(), "{dir}/agents should be symlink");
            assert!(s.join("a.md").exists(), "{dir}/skills/a.md should exist");
            assert!(a.join("b.md").exists(), "{dir}/agents/b.md should exist");
        }
    }
    /// 跨 agent store 链接冲突防线（P1 回归锁）：共享工作区先后由 A、B 接管时，
    /// B 的重链会让 A 的技能被静默覆盖——防线在此场景 fail-fast；同 agent 重入
    /// 与无链工作区幂等放行（manifest 并集视图复刻前的过渡防线）。
    #[cfg(unix)]
    #[tokio::test]
    async fn cross_agent_link_conflict_rejected_same_agent_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).await.unwrap();

        // 无链工作区（首次创建）→ 放行
        assert!(
            detect_cross_agent_link_conflict(&ws, "agent-a")
                .await
                .is_ok()
        );

        // 建链到 agent-a 的 store（模拟 A 已创建工作区；链接目标含
        // `.agent-store/{agentId}` 段——本机制建立的形态）
        let store_a = tmp
            .path()
            .join("u1")
            .join(".agent-store")
            .join("agent-a")
            .join("skills");
        fs::create_dir_all(&store_a).await.unwrap();
        let link = ws.join(".agents").join("skills");
        fs::create_dir_all(link.parent().unwrap()).await.unwrap();
        fs::symlink(
            pathdiff::diff_paths(&store_a, link.parent().unwrap()).unwrap(),
            &link,
        )
        .await
        .unwrap();

        // 同 agent 重入 → 幂等放行
        assert!(
            detect_cross_agent_link_conflict(&ws, "agent-a")
                .await
                .is_ok()
        );

        // 不同 agent 接管 → 拒绝（防静默覆盖）
        let err = detect_cross_agent_link_conflict(&ws, "agent-b")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("agent skill store conflict"), "{msg}");
        assert!(msg.contains("agent-a") && msg.contains("agent-b"), "{msg}");

        // 异形链（目标不含 .agent-store 段，非本机制建立）→ 不误伤放行
        let ws3 = tmp.path().join("ws3");
        fs::create_dir_all(ws3.join(".agents")).await.unwrap();
        let foreign = tmp.path().join("foreign-skills");
        fs::create_dir_all(&foreign).await.unwrap();
        fs::symlink(
            pathdiff::diff_paths(&foreign, ws3.join(".agents")).unwrap(),
            ws3.join(".agents").join("skills"),
        )
        .await
        .unwrap();
        assert!(
            detect_cross_agent_link_conflict(&ws3, "agent-b")
                .await
                .is_ok(),
            "foreign-shaped link must not be rejected"
        );

        // 相对链解析出的 owner 是 .agent-store 段的直接下一级（store 布局形态）
        let ws2 = tmp.path().join("ws2");
        fs::create_dir_all(ws2.join(".agents")).await.unwrap();
        let user_root = tmp.path().join("root").join("u1");
        let store = user_root
            .join(".agent-store")
            .join("agent-a")
            .join("skills");
        fs::create_dir_all(&store).await.unwrap();
        fs::symlink(
            pathdiff::diff_paths(&store, ws2.join(".agents")).unwrap(),
            ws2.join(".agents").join("skills"),
        )
        .await
        .unwrap();
        assert!(
            detect_cross_agent_link_conflict(&ws2, "agent-b")
                .await
                .is_err(),
            "store 布局形态的跨 agent 同样拒绝"
        );
        assert!(
            detect_cross_agent_link_conflict(&ws2, "agent-a")
                .await
                .is_ok(),
            "store 布局形态的同 agent 放行"
        );
    }

    // ===== 共享技能视图（manifest 并集）=====

    async fn seed_agent_skill(user_root: &Path, agent_id: &str, name: &str, dynamic: bool) {
        let (skills_dir, _) = ensure_agent_store_dirs(user_root, agent_id).await.unwrap();
        let src = skills_dir.join(name);
        fs::create_dir_all(&src).await.unwrap();
        fs::write(src.join("SKILL.md"), format!("{agent_id}/{name}"))
            .await
            .unwrap();
        if dynamic {
            ensure_dynamic_add_lock(&src).await.unwrap();
        }
    }

    async fn view_entry_names(workspace: &Path, sub: &str) -> Vec<String> {
        let mount = workspace.join(".agents").join(sub);
        let mut names = Vec::new();
        if let Ok(mut rd) = fs::read_dir(&mount).await {
            while let Ok(Some(entry)) = rd.next_entry().await {
                names.push(entry.file_name().to_string_lossy().to_string());
            }
        }
        names.sort();
        names
    }

    #[tokio::test]
    async fn shared_view_unions_multiple_agents_without_conflict() {
        // 修复前（目录级整链 + 跨 agent 防线）：agent-b 对同一共享工作区会被
        // fail-fast 拒绝；manifest 并集视图下两 agent 技能并存
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        seed_agent_skill(&user_root, "agent-a", "alpha", false).await;
        seed_agent_skill(&user_root, "agent-b", "beta", false).await;

        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(vec!["alpha".into()]),
                subagents: Some(vec![]),
            },
        )
        .await
        .unwrap();
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-b",
            "proj",
            SharedSkillLists {
                skills: Some(vec!["beta".into()]),
                subagents: Some(vec![]),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            view_entry_names(&workspace, "skills").await,
            vec!["alpha", "beta"],
            "并集视图同时含两 agent 技能"
        );
        // manifest 记录两 agent 引用
        let manifest = read_manifest(&project_store_root(&user_root, "proj").unwrap())
            .await
            .unwrap();
        assert_eq!(manifest.agents.len(), 2);
        assert_eq!(manifest.agents["agent-a"].skills, vec!["alpha".to_string()]);
        assert_eq!(manifest.agents["agent-b"].skills, vec!["beta".to_string()]);
        // 挂载结构：.agents 实体目录 + 其余 ACP 目录内链
        assert!(workspace.join(".agents").join("skills").is_dir());
        for dir in crate::service::skills::ALL_AGENT_DIRS {
            if *dir == ".agents" {
                continue;
            }
            let link = workspace.join(dir).join("skills");
            assert!(is_dir_link(&link), "{} 内链存在", dir);
            assert!(
                fs::read_link(&link)
                    .await
                    .unwrap()
                    .ends_with(".agents/skills"),
                "{dir} 内链指向主目录"
            );
        }
    }

    #[tokio::test]
    async fn shared_view_removes_entry_only_when_refs_reach_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        seed_agent_skill(&user_root, "agent-a", "alpha", false).await;
        seed_agent_skill(&user_root, "agent-b", "alpha", false).await;

        for agent in ["agent-a", "agent-b"] {
            sync_shared_skill_view(
                &user_root,
                &workspace,
                agent,
                "proj",
                SharedSkillLists {
                    skills: Some(vec!["alpha".into()]),
                    subagents: None,
                },
            )
            .await
            .unwrap();
        }
        assert_eq!(view_entry_names(&workspace, "skills").await, vec!["alpha"]);

        // agent-a 引用归零：agent-b 仍引用 → 保留
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(vec![]),
                subagents: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            view_entry_names(&workspace, "skills").await,
            vec!["alpha"],
            "仍有 agent 引用时保留"
        );

        // agent-b 也归零 → 移除
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-b",
            "proj",
            SharedSkillLists {
                skills: Some(vec![]),
                subagents: None,
            },
        )
        .await
        .unwrap();
        assert!(
            view_entry_names(&workspace, "skills").await.is_empty(),
            "引用全部归零后移除"
        );
    }

    #[tokio::test]
    async fn shared_view_keeps_dynamic_skills_outside_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        seed_agent_skill(&user_root, "agent-a", "configured", false).await;
        seed_agent_skill(&user_root, "agent-a", "dyn-skill", true).await;

        // 动态技能不进 manifest（清单只写 configured）
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(vec!["configured".into()]),
                subagents: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            view_entry_names(&workspace, "skills").await,
            vec!["configured", "dyn-skill"],
            "动态技能并入并集"
        );

        // 清单清空（校准模式）也不清动态条目
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(vec![]),
                subagents: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            view_entry_names(&workspace, "skills").await,
            vec!["dyn-skill"],
            "动态条目不受校准影响"
        );

        // 其他 agent 的同步也不清它（动态锁保护跨 agent 校准）
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-b",
            "proj",
            SharedSkillLists {
                skills: Some(vec![]),
                subagents: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            view_entry_names(&workspace, "skills").await,
            vec!["dyn-skill"]
        );
    }

    #[tokio::test]
    async fn shared_view_push_mode_without_lists_only_calibrates() {
        // push-skills 自愈模式（清单 None）：不更新 manifest、只校准视图
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        seed_agent_skill(&user_root, "agent-a", "alpha", false).await;
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(vec!["alpha".into()]),
                subagents: None,
            },
        )
        .await
        .unwrap();
        // agent-b 不带清单同步：其引用不写入（不会把引用集改写为空）
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-b",
            "proj",
            SharedSkillLists::default(),
        )
        .await
        .unwrap();
        let manifest = read_manifest(&project_store_root(&user_root, "proj").unwrap())
            .await
            .unwrap();
        // TS 同款：同步者条目无条件落盘（空清单条目无引用，无害），但引用集
        // 不被改写——alpha 仍由 agent-a 引用，视图不被清空
        assert_eq!(
            manifest.agents["agent-b"],
            SkillViewEntry::default(),
            "未传清单时新增条目为空（不产生引用）"
        );
        assert_eq!(
            manifest.agents["agent-a"].skills,
            vec!["alpha".to_string()],
            "既有引用未被改写"
        );
        assert_eq!(
            view_entry_names(&workspace, "skills").await,
            vec!["alpha"],
            "引用集未被清空"
        );
    }

    #[tokio::test]
    async fn shared_view_corrupt_manifest_fails_fast() {
        // 有意偏离 TS（catch → 空表）：损坏即报错——空表会把其他 agent 的
        // 引用全部清出视图
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        let store_root = project_store_root(&user_root, "proj").unwrap();
        fs::create_dir_all(&store_root).await.unwrap();
        fs::write(store_root.join(MANIFEST_FILE), "{ not json")
            .await
            .unwrap();
        let result = sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists::default(),
        )
        .await;
        assert!(result.is_err(), "损坏 manifest 必须 fail-fast");
    }

    #[tokio::test]
    async fn shared_view_clears_orphan_links_when_entities_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        seed_agent_skill(&user_root, "agent-a", "alpha", false).await;
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(vec!["alpha".into()]),
                subagents: None,
            },
        )
        .await
        .unwrap();
        // 实体被外部删除（各 agent prune 已清）；引用还在 → 孤儿链清理
        fs::remove_dir_all(
            agent_store_path(&user_root, "agent-a")
                .unwrap()
                .join("skills/alpha"),
        )
        .await
        .unwrap();
        sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists::default(),
        )
        .await
        .unwrap();
        assert!(
            view_entry_names(&workspace, "skills").await.is_empty(),
            "实体全缺时清孤儿链"
        );
    }

    // ===== F01 数据保护回归：清单名/ID 路径穿越必须在任何写/删除前被拒绝 =====

    #[test]
    fn validate_store_segment_rejects_traversal_and_accepts_normal_names() {
        // 平台原生 components() 判定 + 规范形态不变式（Q01）：唯一 Normal 段
        // 必须与输入完全相等——会被归一化改变形态的输入（"a/"、"a/."、前后
        // 空白包围点段）一律拒绝，杜绝"校验一种值、使用另一种值"。
        for bad in [
            "",
            ".",
            "..",
            " ..",
            ".. ",
            " . ",
            "./a",
            "/absolute",
            "a/",
            "a/.",
            "a/b",
            "../../victim",
            "a\0b",
        ] {
            assert!(
                validate_store_segment("skill name", bad).is_err(),
                "must reject {bad:?}"
            );
        }
        // Windows 分隔符/盘符在 win 构建下由 components() 识别为多段/前缀；
        // 容器内 Linux 运行时它们是合法单段文件名，不放进来回测试
        #[cfg(windows)]
        for bad in ["a\\b", "..\\..\\victim", "C:evil"] {
            assert!(
                validate_store_segment("skill name", bad).is_err(),
                "must reject {bad:?} on Windows"
            );
        }
        for ok in ["alpha", "my-skill_1.2", "技能", "sub agent name"] {
            assert!(
                validate_store_segment("skill name", ok).is_ok(),
                "must accept {ok:?}"
            );
        }
    }

    #[tokio::test]
    async fn install_skill_dir_rejects_escaping_name_without_touching_target() {
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        // 受害目录在 store 同层（= dest_skills_dir/../../victim 解析目标）
        let victim = tmp.path().join("victim");
        fs::create_dir_all(&victim).await.unwrap();
        fs::write(victim.join("data.txt"), "keep").await.unwrap();

        let src = tmp.path().join("src");
        fs::create_dir_all(&src).await.unwrap();
        let skills_dir = user_root
            .join(".agent-store")
            .join("agent-a")
            .join("skills");
        fs::create_dir_all(&skills_dir).await.unwrap();

        let result = install_skill_dir(&src, &skills_dir, "../../victim", false).await;
        assert!(result.is_err(), "escaping skill name must be rejected");
        assert_eq!(
            fs::read_to_string(victim.join("data.txt")).await.unwrap(),
            "keep",
            "受害者目录内容必须原样保留"
        );
    }

    #[tokio::test]
    async fn poisoned_manifest_name_is_rejected_and_deletes_nothing() {
        // 恶意/损坏清单：名字为 `../../victim`——修复前会在视图校准
        // "实体缺失清孤儿链"分支把它 join 后递归删除工作区外目录
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        let victim = tmp.path().join("victim");
        fs::create_dir_all(&victim).await.unwrap();
        fs::write(victim.join("data.txt"), "keep").await.unwrap();

        let store_root = project_store_root(&user_root, "proj").unwrap();
        fs::create_dir_all(&store_root).await.unwrap();
        fs::write(
            store_root.join(MANIFEST_FILE),
            r#"{"agents":{"agent-a":{"skills":["../../victim"],"subagents":[]}}}"#,
        )
        .await
        .unwrap();

        let result = sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists::default(),
        )
        .await;
        assert!(result.is_err(), "被污染 manifest 必须整体拒绝");
        assert_eq!(
            fs::read_to_string(victim.join("data.txt")).await.unwrap(),
            "keep",
            "受管范围外目录必须原样保留"
        );
    }

    #[tokio::test]
    async fn view_lock_takeover_and_late_release_never_delete_successor_lock() {
        // Q07：A 持锁（伪造超龄 mtime）→ B 接管（token 轮换）→ A 的迟到
        // Drop 不得删掉 B 的锁（C 不得凭空进入）；B 正常释放有效
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj-store");
        fs::create_dir_all(&root).await.unwrap();
        let lock = root.join(VIEW_LOCK_NAME);
        // A 持锁
        let a = ViewGuard::acquire(&root).await.expect("A acquires");
        // 伪造超龄：直接回写 mtime（文件锁内容为 A token）
        let old =
            std::time::SystemTime::now() - std::time::Duration::from_secs(VIEW_LOCK_STALE_MS + 60);
        let f = std::fs::File::options().write(true).open(&lock).unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        // B 接管（stale 检测 → token 轮换 rename）
        let b = ViewGuard::acquire(&root).await.expect("B takes over");
        assert_ne!(a.token, b.token, "接管必须轮换所有权 token");
        // A 迟到 Drop：锁内容是 B 的 token——不得删除
        drop(a);
        assert!(lock.exists(), "迟到 Drop 不得删除接管者的锁（Q07）");
        assert_eq!(
            fs::read_to_string(&lock).await.unwrap(),
            b.token,
            "锁内容必须仍是 B 的所有权 token"
        );
        // B 正常释放有效
        drop(b);
        assert!(!lock.exists(), "持有者自身释放必须生效");
    }

    #[tokio::test]
    async fn trimmed_dot_segments_rejected_before_manifest_write() {
        // Q01：原始串（".. "）是合法 Normal 组件，但 trim 后变成 ".."——
        // 规范化后的值必须在写 manifest 前被拒，不能把 ".." 落盘参与视图操作
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        let result = sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(vec![".. ".to_string(), "normal".to_string()]),
                subagents: None,
            },
        )
        .await;
        assert!(result.is_err(), "trim 后非法的名字必须整单拒绝");
        let store_root = project_store_root(&user_root, "proj").unwrap();
        let manifest = read_manifest(&store_root).await.unwrap();
        assert!(
            manifest.agents.is_empty(),
            "非法清单不得写入 manifest: {manifest:?}"
        );
    }

    #[tokio::test]
    async fn replaced_non_agents_acp_roots_are_refused_across_all_variants() {
        // V05：.agents 之外的 ACP 根（SYNC_TARGET_DIRS 任一）被替换为指向
        // victim 的链接——sync 与 link 两条路径都必须拒绝，
        // 且 victim、manifest、其余视图零变更
        for replaced in crate::service::skills::SYNC_TARGET_DIRS {
            let tmp = tempfile::tempdir().unwrap();
            let user_root = tmp.path().join("u1");
            let workspace = tmp.path().join("ws");
            fs::create_dir_all(&workspace).await.unwrap();
            let victim = tmp.path().join("victim");
            fs::create_dir_all(victim.join("skills").join("keep"))
                .await
                .unwrap();
            fs::write(victim.join("skills").join("keep").join("SKILL.md"), "keep")
                .await
                .unwrap();
            // 受害目录挂在被替换的根上
            #[cfg(unix)]
            std::os::unix::fs::symlink(&victim, workspace.join(replaced)).unwrap();

            let sync_result = sync_shared_skill_view(
                &user_root,
                &workspace,
                "agent-a",
                "proj",
                SharedSkillLists {
                    skills: Some(Vec::new()),
                    subagents: None,
                },
            )
            .await;
            let link_result = link_workspace_to_agent_store(
                &workspace,
                &user_root
                    .join(".agent-store")
                    .join("agent-a")
                    .join("skills"),
                &user_root
                    .join(".agent-store")
                    .join("agent-a")
                    .join("agents"),
            )
            .await;
            #[cfg(unix)]
            {
                assert!(
                    sync_result.is_err(),
                    "{replaced} 根被链接替换时 sync 必须拒绝"
                );
                assert!(
                    link_result.is_err(),
                    "{replaced} 根被链接替换时 link 必须拒绝"
                );
                assert_eq!(
                    fs::read_to_string(victim.join("skills").join("keep").join("SKILL.md"))
                        .await
                        .unwrap(),
                    "keep",
                    "{replaced}: victim 内容必须原样保留"
                );
                let store_root = project_store_root(&user_root, "proj").unwrap();
                let manifest = read_manifest(&store_root).await.unwrap();
                assert!(
                    manifest.agents.is_empty(),
                    "{replaced}: 非法清单不得写入 manifest"
                );
            }
            #[cfg(not(unix))]
            let _ = (sync_result, link_result);
        }
    }

    #[tokio::test]
    async fn replaced_agents_root_symlink_is_refused_without_touching_target() {
        // Q02：workspace/.agents 被替换为指向 victim 的链接——同步必须拒绝，
        // 不得沿链接对 victim 内条目做任何删除/写入
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).await.unwrap();
        let victim = tmp.path().join("victim");
        fs::create_dir_all(victim.join("skills").join("keep"))
            .await
            .unwrap();
        fs::write(victim.join("skills").join("keep").join("SKILL.md"), "keep")
            .await
            .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, workspace.join(".agents")).unwrap();

        let result = sync_shared_skill_view(
            &user_root,
            &workspace,
            "agent-a",
            "proj",
            SharedSkillLists {
                skills: Some(Vec::new()),
                subagents: None,
            },
        )
        .await;
        #[cfg(unix)]
        {
            assert!(result.is_err(), "受管根被链接替换时必须拒绝");
            assert_eq!(
                fs::read_to_string(victim.join("skills").join("keep").join("SKILL.md"))
                    .await
                    .unwrap(),
                "keep",
                "链接目标内容必须原样保留"
            );
        }
        #[cfg(not(unix))]
        let _ = result;
    }

    #[tokio::test]
    async fn malicious_skill_names_rejected_at_service_entry_without_mutation() {
        // create-workspace-v2 全链入口级防线：恶意 skillNames 在任何目录
        // 创建/写入前被 400 拒绝
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("u1");
        let session_workspace = user_root.join("ws");
        let result = crate::service::computer_ws::create_workspace_with_agent_store(
            crate::service::computer_ws::CreateAgentStoreParams {
                user_root: &user_root,
                session_workspace: &session_workspace,
                agent_id: "agent-a",
                skill_zip: None,
                skill_urls: Vec::new(),
                skill_url_map: None,
                skill_names: Some(vec!["../../victim".to_string()]),
                update_skill_names: None,
                hook_config: None,
                downloader: None,
                shared_project_id: None,
            },
        )
        .await;
        assert!(result.is_err(), "恶意 skillNames 必须被拒绝");
        assert!(
            !session_workspace.exists(),
            "校验失败不得留下任何已创建目录"
        );
        assert!(!user_root.join(".agent-store").exists(), "store 不得被创建");
    }
}
