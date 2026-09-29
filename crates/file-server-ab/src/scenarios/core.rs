//! core 套件场景定义（computer/project 文件与边界用例）。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{Value, json};

use crate::scenarios::*;
use crate::types::*;
use crate::util::*;

pub(crate) fn core_scenarios(fixtures: &Path) -> Result<Vec<(&'static str, RequestSpec)>> {
    let json_request = |method: Method, path: String, body: Value| -> Result<RequestSpec> {
        Ok(RequestSpec {
            method,
            path,
            body: serde_json::to_vec(&body).context("serialize A/B request body")?,
            content_type: Some("application/json"),
            headers: BTreeMap::new(),
            health_probe: false,
            expected_status: ExpectedStatus::Success2xx,
            normalized_paths: Vec::new(),
            normalized_headers: Vec::new(),
            timeout: Duration::from_secs(30),
        })
    };
    let get = |path: String| RequestSpec {
        method: Method::GET,
        path,
        body: Vec::new(),
        content_type: None,
        headers: BTreeMap::new(),
        health_probe: false,
        expected_status: ExpectedStatus::Success2xx,
        normalized_paths: Vec::new(),
        normalized_headers: Vec::new(),
        timeout: Duration::from_secs(30),
    };
    let get_client_error = |path: String| RequestSpec {
        expected_status: ExpectedStatus::ClientError4xx,
        normalized_paths: vec!["/error/requestId".into(), "/error/timestamp".into()],
        normalized_headers: Vec::new(),
        ..get(path)
    };
    let user = CASE_USER;
    let cid = CASE_CID;
    let root = "/data/computer-workspace/file-server-ab-user/file-server-ab-session";
    let skills_zip =
        fs::read(fixtures.join("skills-fixture.zip")).context("read local skills A/B fixture")?;
    let workspace_zip = fs::read(fixtures.join("workspace-project.zip"))
        .context("read local workspace ZIP A/B fixture")?;
    let package_zip = fs::read(fixtures.join("package-project.zip"))
        .context("read local package-project A/B fixture")?;
    let mut file_meta = json_request(
        Method::POST,
        "/api/computer/get-file-meta".to_string(),
        json!({
            "userId": user,
            "cId": cid,
            "filePaths": ["README.md", "sub/nested/hello.txt"]
        }),
    )?;
    // Separate fixture trees are created milliseconds apart. Compare every stable metadata
    // field and validate that both timestamps are present; preserve their raw values in bodies.
    file_meta.normalized_paths = vec!["/metas/0/mtimeMs".into(), "/metas/1/mtimeMs".into()];
    let boundary_meta = json_request(
        Method::POST,
        "/api/computer/get-file-meta".to_string(),
        json!({
            "userId": user,
            "cId": cid,
            "filePaths": [".hidden.txt", "  中文文件 .txt  ", "binary.bin"]
        }),
    )?;
    let mut boundary_meta = boundary_meta;
    // These files are created independently on each side. Preserve their raw mtimes in
    // requests.jsonl, but don't mistake millisecond creation-time skew for API drift.
    // metas/1 is the leading/trailing-space file name: Rust addresses the exact path
    // and returns full metadata; TypeScript trims the name and reports ENOENT. That
    // divergence is approved (exact file names win), so the whole entry is compared
    // semantically with per-side assertions below instead of field-by-field.
    boundary_meta.normalized_paths = vec![
        "/metas/0/mtimeMs".into(),
        "/metas/2/mtimeMs".into(),
        "/metas/1".into(),
    ];
    let single_upload_bytes: &[u8] = &[0, 1, 2, 13, 10, 127, 255];
    let single_upload_body = multipart_form_body(
        AB_MULTIPART_BOUNDARY,
        &[
            ("userId", user),
            ("cId", cid),
            ("filePath", "ab-transfer/single/nested/payload.bin"),
        ],
        &[("file", "payload.bin", single_upload_bytes)],
    );
    let batch_first_bytes: &[u8] = b"batch upload one\n";
    let batch_second_bytes: &[u8] = &[0, 255, 10, 13, 42];
    let batch_upload_body = multipart_form_body(
        AB_MULTIPART_BOUNDARY,
        &[
            ("userId", user),
            ("cId", cid),
            ("filePaths", "ab-transfer/batch/one.txt"),
            ("filePaths", "ab-transfer/batch/nested/two.bin"),
        ],
        &[
            ("files", "one.txt", batch_first_bytes),
            ("files", "two.bin", batch_second_bytes),
        ],
    );
    let static_read =
        get("/api/page/static/file-server-ab-react/src/file-server-ab.txt".to_string());
    // etag/last-modified 由 header 协议语义层校验 (静态内容要求两侧存在且格式合法,
    // 值可不同), 不再整头跳过比较。
    let mut static_range = get("/api/page/static/file-server-ab-react/package.json".to_string());
    static_range
        .headers
        .insert("range".into(), "bytes=0-5".into());
    static_range.expected_status = ExpectedStatus::PartialContent206;
    let mut scenarios = vec![
        (
            "health",
            RequestSpec {
                method: Method::GET,
                path: "/health".to_string(),
                body: Vec::new(),
                content_type: None,
                headers: BTreeMap::new(),
                health_probe: true,
                expected_status: ExpectedStatus::Success2xx,
                normalized_paths: Vec::new(),
                normalized_headers: Vec::new(),
                timeout: Duration::from_secs(30),
            },
        ),
        ("root", get("/".to_string())),
        // 版本是各端契约线声明 (Java 据此做能力门禁)，值不要求跨实现相等；
        // 由断言校验语义版本形状。
        (
            "version",
            RequestSpec {
                normalized_paths: vec!["/version".into()],
                ..get("/api/version".to_string())
            },
        ),
        (
            "create-react-template-project",
            project_create_spec("file-server-ab-react", "react")?,
        ),
        (
            "read-react-template-project",
            get("/api/project/get-project-content?projectId=file-server-ab-react&proxyPath=%2Fproxy".to_string()),
        ),
        (
            "update-project-file",
            json_request(
                Method::POST,
                "/api/project/specified-files-update".to_string(),
                json!({
                    "projectId":"file-server-ab-react",
                    "codeVersion":"1",
                    "files":[{"operation":"create","name":"src/file-server-ab.txt","contents":"A%2FB+comparison%0A"}]
                }),
            )?,
        ),
        (
            "read-project-static-file",
            static_read,
        ),
        ("read-project-static-file-range", static_range),
        (
            "create-vue-template-project",
            project_create_spec("file-server-ab-vue", "vue3")?,
        ),
        (
            "read-vue-template-project",
            get("/api/project/get-project-content?projectId=file-server-ab-vue&proxyPath=%2Fproxy".to_string()),
        ),
        (
            "computer-file-list-all",
            get(format!("/api/computer/get-file-list?userId={user}&cId={cid}&proxyPath=%2Fproxy")),
        ),
        (
            "computer-file-list-boundary-fixtures",
            get(format!("/api/computer/get-file-list?userId={user}&cId={cid}&type=file&proxyPath=%2Fproxy")),
        ),
        (
            "computer-file-list-files-limited",
            get(format!("/api/computer/get-file-list?userId={user}&cId={cid}&type=file&limit=2")),
        ),
        (
            "computer-file-list-empty-directories",
            get(format!("/api/computer/get-file-list?userId={user}&cId={cid}&type=dir")),
        ),
        (
            "computer-file-list-subdirectory",
            get(format!("/api/computer/get-file-list?userId={user}&cId={cid}&relativePath=sub")),
        ),
        (
            "computer-resolve-file-existing",
            get(format!(
                "/api/computer/resolve-file?userId={user}&cId={cid}&filePath=sub%2Fnested%2Fhello.txt&proxyPath=%2Fproxy"
            )),
        ),
        (
            "computer-resolve-file-hidden",
            get(format!(
                "/api/computer/resolve-file?userId={user}&cId={cid}&filePath={}&proxyPath=%2Fproxy",
                encode_query(".hidden.txt")
            )),
        ),
        (
            "computer-resolve-file-symlink-inside",
            get(format!(
                "/api/computer/resolve-file?userId={user}&cId={cid}&filePath={}&proxyPath=%2Fproxy",
                encode_query("inside-link.txt")
            )),
        ),
        (
            "computer-resolve-file-symlink-outside-root",
            get(format!(
                "/api/computer/resolve-file?userId={user}&cId={cid}&filePath={}&proxyPath=%2Fproxy",
                encode_query("outside-link.txt")
            )),
        ),
        // ── TS e822516 (1.5.7) escapesRoot: 越界目录链接的中段逃逸 ──────────────────
        // relativePath 指进 outside-dir (→ 会话根之外的工作区根) 双侧 400;
        // message 文案 TS 中文 / Rust 英文 (默认英文错误提示), 归一化对比类型与 details。
        (
            "computer-file-list-relative-path-outside-dir-link",
            {
                let mut spec = get_client_error(format!(
                    "/api/computer/get-file-list?userId={user}&cId={cid}&relativePath=outside-dir"
                ));
                spec.normalized_paths.push("/error/message".into());
                spec
            },
        ),
        (
            "computer-search-relative-path-outside-dir-link",
            {
                // limit/maxVisit/timeoutMs 为必填 query —— 首跑漏传导致两侧各自
                // 报 limit 参数错误, 边界检查根本没被打到
                let mut spec = get_client_error(format!(
                    "/api/computer/search-files?userId={user}&cId={cid}&kw=hello&limit=10&maxVisit=100&timeoutMs=1000&relativePath=outside-dir"
                ));
                spec.normalized_paths.push("/error/message".into());
                spec
            },
        ),
        // 静态直读经越界目录链接 → 双侧 404 纯文本 "Not Found" (不泄露内容与大小);
        // 框架默认 content-type 不同 (express text/html vs axum text/plain), 归一化。
        (
            "computer-static-dir-link-outside-root",
            {
                let mut spec = get_client_error(format!(
                    "/api/computer/static/{user}/{cid}/outside-dir/file-server-ab-outside-secret.txt"
                ));
                spec.normalized_headers.push("content-type".into());
                spec
            },
        ),
        // get-file-meta 经越界目录链接 → 双侧 200 + 单条 error:"illegal path"
        (
            "computer-get-file-meta-outside-dir-link",
            {
                // 两侧容器各自的 fixture 副本创建时间不同, metas[1] 的 mtimeMs
                // 归一化 (与既有 computer-file-meta 用例同款); 只校验存在性与
                // 稳定字段, 原始值保留在 bodies 证据里。
                let mut spec = json_request(
                    Method::POST,
                    "/api/computer/get-file-meta".to_string(),
                    json!({
                        "userId": user,
                        "cId": cid,
                        "filePaths": ["outside-dir/file-server-ab-outside-secret.txt", "sub/nested/hello.txt"]
                    }),
                )?;
                spec.normalized_paths.push("/metas/1/mtimeMs".into());
                spec
            },
        ),
        (
            "computer-resolve-file-path-traversal",
            get(format!(
                "/api/computer/resolve-file?userId={user}&cId={cid}&filePath={}&proxyPath=%2Fproxy",
                encode_query("../../file-server-ab-outside-secret.txt")
            )),
        ),
        (
            "computer-search-files",
            get(format!(
                "/api/computer/search-files?userId={user}&cId={cid}&kw=hello&limit=10&maxVisit=100&timeoutMs=1000"
            )),
        ),
        ("computer-file-meta", file_meta),
        ("computer-file-meta-boundary", boundary_meta),
        (
            "computer-files-update-mixed-operations",
            json_request(
                Method::POST,
                "/api/computer/files-update".to_string(),
                json!({
                    "userId": user,
                    "cId": cid,
                    "files": [
                        {"operation":"create", "name":"ab-write", "isDir":true},
                        {"operation":"create", "name":"ab-write/created.txt", "contents":"A%2FB%2B%25%E4%B8%AD%0A"},
                        {"operation":"modify", "name":"README.md", "contents":"Updated%20README%0A"},
                        {"operation":"rename", "name":"ab-write/renamed.txt", "renameFrom":"ab-write/created.txt"},
                        {"operation":"delete", "name":"sub/nested/hello.txt"}
                    ]
                }),
            )?,
        ),
        (
            "computer-read-files-update-content",
            get(format!(
                "/api/computer/static/{user}/{cid}/ab-write/renamed.txt"
            )),
        ),
        (
            "computer-upload-file-binary",
            RequestSpec {
                method: Method::POST,
                path: "/api/computer/upload-file".to_string(),
                body: single_upload_body,
                content_type: Some("multipart/form-data; boundary=----file-server-ab-boundary-6d86f5"),
                headers: BTreeMap::new(),
                health_probe: false,
                expected_status: ExpectedStatus::Success2xx,
                normalized_paths: Vec::new(),
                normalized_headers: Vec::new(),
                timeout: Duration::from_secs(30),
            },
        ),
        (
            "computer-read-uploaded-single-binary",
            get(format!(
                "/api/computer/static/{user}/{cid}/ab-transfer/single/nested/payload.bin"
            )),
        ),
        (
            "computer-upload-files-mixed-content",
            RequestSpec {
                method: Method::POST,
                path: "/api/computer/upload-files".to_string(),
                body: batch_upload_body,
                content_type: Some("multipart/form-data; boundary=----file-server-ab-boundary-6d86f5"),
                headers: BTreeMap::new(),
                health_probe: false,
                expected_status: ExpectedStatus::Success2xx,
                normalized_paths: Vec::new(),
                normalized_headers: Vec::new(),
                timeout: Duration::from_secs(30),
            },
        ),
        (
            "computer-read-uploaded-batch-binary",
            get(format!(
                "/api/computer/static/{user}/{cid}/ab-transfer/batch/nested/two.bin"
            )),
        ),
        (
            "computer-file-list-invalid-type",
            get_client_error(format!(
                "/api/computer/get-file-list?userId={user}&cId={cid}&type=invalid"
            )),
        ),
        (
            "computer-file-list-invalid-limit",
            get_client_error(format!(
                "/api/computer/get-file-list?userId={user}&cId={cid}&limit=-1"
            )),
        ),
        (
            "computer-file-list-depth-two",
            get(format!(
                "/api/computer/get-file-list?userId={user}&cId={cid}&recursive=false&depth=2"
            )),
        ),
        (
            "computer-file-list-depth-file-type-descends",
            get(format!(
                "/api/computer/get-file-list?userId={user}&cId={cid}&recursive=false&depth=2&type=file"
            )),
        ),
        (
            "computer-file-list-depth-invalid",
            get_client_error(format!(
                "/api/computer/get-file-list?userId={user}&cId={cid}&recursive=false&depth=11"
            )),
        ),
        // TS cd0f075 (1.5.6) 收紧为 /^\d+$/ 严格十进制: 小数形式与带空白的
        // 输入两侧都是 400（Rust 曾按旧 Number 语义接受 "3.0"/" 3 "，此为回归锚点）
        (
            "computer-file-list-depth-decimal-rejected",
            get_client_error(format!(
                "/api/computer/get-file-list?userId={user}&cId={cid}&recursive=false&depth=3.0"
            )),
        ),
        (
            "computer-file-list-limit-whitespace-rejected",
            get_client_error(format!(
                "/api/computer/get-file-list?userId={user}&cId={cid}&limit=%20%203%20"
            )),
        ),
        (
            "computer-file-list-depth-ignored-in-recursive",
            get(format!(
                "/api/computer/get-file-list?userId={user}&cId={cid}&recursive=true&depth=99"
            )),
        ),
        (
            "computer-search-files-type-file",
            get(format!(
                "/api/computer/search-files?userId={user}&cId={cid}&kw=hello&limit=10&maxVisit=100&timeoutMs=1000&type=file"
            )),
        ),
        (
            "computer-search-files-type-dir",
            get(format!(
                "/api/computer/search-files?userId={user}&cId={cid}&kw=hello&limit=10&maxVisit=100&timeoutMs=1000&type=directory"
            )),
        ),
        (
            "computer-search-files-type-invalid",
            get(format!(
                "/api/computer/search-files?userId={user}&cId={cid}&kw=hello&limit=10&maxVisit=100&timeoutMs=1000&type=bogus"
            )),
        ),
        (
            "fs-roots",
            get("/api/computer/fs/roots".to_string()),
        ),
        (
            "fs-children",
            get(format!("/api/computer/fs/children?path={}", encode_query(root))),
        ),
        (
            "fs-mkdir-preserve-spaces",
            json_request(
                Method::POST,
                "/api/computer/fs/mkdir".to_string(),
                json!({"parentPath":root,"dirName":"  A-B spaced dir  "}),
            )?,
        ),
        (
            "fs-rename-preserve-spaces",
            json_request(
                Method::POST,
                "/api/computer/fs/rename".to_string(),
                json!({"path":format!("{root}/  A-B spaced dir  "),"newName":" renamed dir "}),
            )?,
        ),
    ];

    scenarios.extend(core_lifecycle_scenarios(
        &skills_zip,
        &workspace_zip,
        &package_zip,
    )?);
    Ok(scenarios)
}

pub(crate) fn core_lifecycle_scenarios(
    skills_zip: &[u8],
    workspace_zip: &[u8],
    package_zip: &[u8],
) -> Result<Vec<(&'static str, RequestSpec)>> {
    let mut scenarios = Vec::new();
    let user = CASE_USER;
    let project = "file-server-ab-project-lifecycle";
    let upload_project = "file-server-ab-project-upload";
    let delete_project = "file-server-ab-project-delete";

    // Project fixtures are created through the public API, then mutated and consumed in order.
    for (case, project_id, template_type) in [
        ("project-fixture-lifecycle-create", project, "react"),
        ("project-fixture-upload-create", upload_project, "react"),
        ("project-fixture-delete-create", delete_project, "vue3"),
    ] {
        scenarios.push((case, project_create_spec(project_id, template_type)?));
    }

    let skills_fields = [("userId", user), ("cId", AB_SKILLS_V1_CID)];
    scenarios.push((
        "computer-create-workspace-v1-local-skills",
        multipart_spec(
            "/api/computer/create-workspace",
            &skills_fields,
            &[("file", "skills-fixture.zip", skills_zip)],
        ),
    ));
    scenarios.push((
        "computer-push-skills-v1-local-zip",
        multipart_spec(
            "/api/computer/push-skills-to-workspace",
            &skills_fields,
            &[("file", "skills-fixture.zip", skills_zip)],
        ),
    ));

    let v2_fields = [
        ("userId", user),
        ("cId", AB_SKILLS_V2_CID),
        ("agentId", "ab-agent"),
        ("skillUrls", "[]"),
        ("skillNames", "[\"file-server-ab-skill\"]"),
        ("mcpServersConfig", "{\"servers\":[]}"),
        ("hooksConfig", "{\"hooks\":[]}"),
        ("permissionsConfig", "{\"allow\":[]}"),
        ("hookScripts", "[]"),
    ];
    scenarios.push((
        "computer-create-workspace-v2-local-config",
        multipart_spec(
            "/api/computer/create-workspace-v2",
            &v2_fields,
            &[("file", "skills-fixture.zip", skills_zip)],
        ),
    ));
    let v2_push_fields = [
        ("userId", user),
        ("cId", AB_SKILLS_V2_CID),
        ("agentId", "ab-agent"),
        ("skillUrls", "[]"),
    ];
    scenarios.push((
        "computer-push-skills-v2-local-zip",
        multipart_spec(
            "/api/computer/push-skills-to-workspace-v2",
            &v2_push_fields,
            &[("file", "skills-fixture.zip", skills_zip)],
        ),
    ));

    scenarios.push((
        "computer-generate-file-utf8",
        json_spec(
            Method::POST,
            "/api/computer/generate-file",
            json!({
                "userId": user,
                "cId": CASE_CID,
                "fileName": "ab-generated/nested/message.txt",
                "content": "A/B + % 中文\n"
            }),
        )?,
    ));
    scenarios.push((
        "computer-read-generated-file-utf8",
        get_spec(format!(
            "/api/computer/static/{user}/{CASE_CID}/ab-generated/nested/message.txt"
        )),
    ));

    let import_fields = [("userId", user), ("cId", AB_IMPORT_CID)];
    scenarios.push((
        "computer-import-project-local-zip",
        multipart_spec(
            "/api/computer/import-project",
            &import_fields,
            &[("file", "workspace-project.zip", workspace_zip)],
        ),
    ));
    scenarios.push((
        "computer-read-imported-project-file",
        get_spec(format!(
            "/api/computer/static/{user}/{AB_IMPORT_CID}/src/imported.txt"
        )),
    ));
    scenarios.push((
        "computer-import-preservation-contract",
        json_spec(
            Method::POST,
            "/api/computer/execute-command",
            json!({
                "userId": user,
                "cId": AB_IMPORT_CID,
                "command": "test \"$(cat .agents/keep.txt)\" = \"preserved agent data\" && test ! -e .agents/from-archive.txt && printf 'import preservation ok\\n'"
            }),
        )?,
    ));

    let init_fields = [
        ("userId", user),
        ("cId", AB_PACKAGE_CID),
        ("enableGit", "true"),
    ];
    scenarios.push((
        "computer-init-project-template-package-fixture",
        multipart_spec(
            "/api/computer/init-project-template",
            &init_fields,
            &[("file", "package-project.zip", package_zip)],
        ),
    ));
    scenarios.push((
        "computer-init-template-git-tree",
        json_spec(
            Method::POST,
            "/api/computer/execute-command",
            json!({
                "userId": user,
                "cId": AB_PACKAGE_CID,
                "command": "git show -s --format=%T HEAD"
            }),
        )?,
    ));
    scenarios.push((
        "computer-install-empty-typescript-project",
        json_spec(
            Method::POST,
            "/api/computer/install-project",
            json!({
                "userId": user,
                "cId": AB_PACKAGE_CID,
                "programmingLanguage": "typescript"
            }),
        )?,
    ));
    scenarios.push((
        "computer-build-agent-package-synthetic",
        json_spec(
            Method::POST,
            "/api/computer/build-agent-package",
            json!({
                "userId": user,
                "cId": AB_PACKAGE_CID,
                "agentId": "17",
                "version": "1.2.3"
            }),
        )?,
    ));
    scenarios.push((
        "computer-cleanup-build-artifacts",
        json_spec(
            Method::POST,
            "/api/computer/cleanup-build-artifacts",
            json!({ "userId": user, "cId": AB_PACKAGE_CID }),
        )?,
    ));

    scenarios.push((
        "computer-execute-command-fixed-output",
        json_spec(
            Method::POST,
            "/api/computer/execute-command",
            json!({
                "userId": user,
                "cId": CASE_CID,
                "command": "printf 'file-server-ab-command-ok\\n'"
            }),
        )?,
    ));
    scenarios.push((
        "computer-get-logs-tail-lines",
        get_spec(format!(
            "/api/computer/get-logs?userId={user}&cId={AB_LOG_CID}&tailLines=2"
        )),
    ));
    scenarios.push((
        "computer-download-all-files-semantic-zip",
        get_spec(format!(
            "/api/computer/download-all-files?userId={user}&cId={CASE_CID}"
        )),
    ));
    scenarios.push((
        "computer-zip-workspace-semantic",
        json_spec(
            Method::POST,
            "/api/computer/zip-workspace",
            json!({ "userId": user, "cId": CASE_CID, "excludeDirs": ["empty"] }),
        )?,
    ));
    scenarios.push((
        "computer-delete-workspace-owned-fixture",
        json_spec(
            Method::POST,
            "/api/computer/delete-workspace",
            json!({ "userId": user, "cId": AB_SKILLS_V1_CID }),
        )?,
    ));

    scenarios.push((
        "project-all-files-update-seed-obsolete",
        json_spec(
            Method::POST,
            "/api/project/specified-files-update",
            json!({
                "projectId": project,
                "codeVersion": "1",
                "files": [{"operation":"create","name":"ab-obsolete.txt","contents":"remove%20me"}]
            }),
        )?,
    ));
    scenarios.push((
        "project-all-files-update-replace",
        json_spec(
            Method::POST,
            "/api/project/all-files-update",
            json!({
                "projectId": project,
                "codeVersion": "2",
                "files": [
                    { "name": "README.md", "contents": "full%20snapshot%0A", "binary": false },
                    { "name": "src/index.html", "contents": "<main>A%2FB%20project</main>%0A", "binary": false },
                    { "name": "empty.txt", "contents": "", "binary": false },
                    { "name": "pnpm-lock.yaml", "contents": "lockfile%20at%20project%20root%0A", "binary": false },
                    { "name": "packages/frontend/package-lock.json", "contents": "%7B%22lockfileVersion%22%3A3%7D%0A", "binary": false }
                ]
            }),
        )?,
    ));
    let mut removed_file_check = get_spec(format!("/api/page/static/{project}/ab-obsolete.txt"));
    removed_file_check.expected_status = ExpectedStatus::ClientError4xx;
    removed_file_check.normalized_paths =
        vec!["/error/requestId".into(), "/error/timestamp".into()];
    scenarios.push((
        "project-all-files-update-removes-omitted-file",
        removed_file_check,
    ));
    scenarios.push((
        "project-upload-single-file-bytes",
        multipart_spec(
            "/api/project/upload-single-file",
            &[
                ("projectId", project),
                ("codeVersion", "3"),
                ("filePath", "src/single.bin"),
            ],
            &[("file", "single.bin", &[0, 1, 2, 13, 10, 127, 255])],
        ),
    ));
    scenarios.push((
        "project-upload-batch-mixed-bytes",
        multipart_spec(
            "/api/project/upload-batch-files",
            &[
                ("projectId", project),
                ("codeVersion", "4"),
                ("filePaths", "src/batch/one.txt"),
                ("filePaths", "src/batch/two.bin"),
            ],
            &[
                ("files", "one.txt", b"batch project file\n"),
                ("files", "two.bin", &[0, 255, 10, 13, 42]),
            ],
        ),
    ));
    scenarios.push((
        "project-upload-attachment-deterministic-name",
        multipart_spec(
            "/api/project/upload-attachment-file",
            &[("projectId", project), ("fileName", "ab-attachment.txt")],
            &[("file", "ab-attachment.txt", b"attachment fixture\n")],
        ),
    ));
    scenarios.push((
        "project-push-skills-local-zip",
        multipart_spec(
            "/api/project/push-skills-to-workspace",
            &[("projectId", project)],
            &[("file", "skills-fixture.zip", skills_zip)],
        ),
    ));
    scenarios.push((
        "project-backup-deprecated-under-git",
        json_spec(
            Method::POST,
            "/api/project/backup-current-version",
            json!({ "projectId": project, "codeVersion": "4" }),
        )?,
    ));
    scenarios.push((
        "project-get-version-deprecated-under-git",
        get_spec(format!(
            "/api/project/get-project-content-by-version?projectId={project}&codeVersion=4"
        )),
    ));
    scenarios.push((
        "project-rollback-deprecated-under-git",
        json_spec(
            Method::POST,
            "/api/project/rollback-version",
            json!({ "projectId": project, "codeVersion": "4", "rollbackTo": "1" }),
        )?,
    ));
    scenarios.push((
        "project-copy-project-tree",
        json_spec(
            Method::POST,
            "/api/project/copy-project",
            json!({ "sourceProjectId": project, "targetProjectId": "file-server-ab-project-copy" }),
        )?,
    ));
    // 已批准分歧 (2026-09-26 确认): 项目复制不复制源 Git 历史。Rust 副本仅含
    // copy commit; TS 复制 .git 保留源历史再追加 copy commit。提交对象本身含
    // 每次运行不同的 hash/date，无法逐值登记规则——整个列表按形状归一，两侧的
    // 已批准形状由 validate_copy_git_history 分侧断言。
    let mut copied_project_git_log = get_spec(
        "/api/git/log?workspaceType=pageApp&projectId=file-server-ab-project-copy&maxCount=20",
    );
    copied_project_git_log.normalized_paths = vec!["/commits".into(), "/total".into()];
    scenarios.push(("project-copy-git-history", copied_project_git_log));
    let mut export_spec = json_spec(
        Method::POST,
        "/api/project/export-project",
        json!({ "projectId": project, "codeVersion": "4", "exportType": "LATEST" }),
    )?;
    // 已批准 (2026-09-26 确认): POST 下载响应不补发 Express 框架头。浏览器不缓存
    // POST 响应、不对 POST 发 Range 请求、Last-Modified 只是当次生成时间——TS 由
    // sendFile 路径带出的这三个头无消费者，Rust 不模仿；ETag 已由 header 协议
    // 语义层按非静态路由处理。
    export_spec.normalized_headers = vec![
        "accept-ranges".into(),
        "cache-control".into(),
        "last-modified".into(),
    ];
    scenarios.push(("project-export-latest-semantic-zip", export_spec));
    scenarios.push((
        "project-upload-project-wrapper-zip",
        multipart_spec(
            "/api/project/upload-project",
            &[("projectId", upload_project), ("codeVersion", "5")],
            &[("file", "workspace-project.zip", workspace_zip)],
        ),
    ));
    scenarios.push((
        "project-read-uploaded-project-file",
        get_spec(format!(
            "/api/page/static/{upload_project}/src/imported.txt"
        )),
    ));
    scenarios.push((
        "project-delete-owned-fixture",
        get_spec(format!(
            "/api/project/delete-project?projectId={delete_project}"
        )),
    ));

    Ok(scenarios)
}

pub(crate) fn prepare_computer_fixture(root: &Path) -> Result<()> {
    let workspace = root.join("computer-workspace");
    let dir = root
        .join("computer-workspace")
        .join(CASE_USER)
        .join(CASE_CID);
    fs::create_dir_all(dir.join("sub/nested"))?;
    fs::create_dir_all(dir.join("empty"))?;
    fs::write(dir.join("README.md"), "file-server A/B fixture\n")?;
    fs::write(dir.join("sub/nested/hello.txt"), "nested file\n")?;
    fs::write(
        dir.join("  spaced name.txt  "),
        "spaces are part of the file name\n",
    )?;
    fs::write(dir.join(".hidden.txt"), "hidden fixture\n")?;
    fs::write(dir.join(".gitignore"), "node_modules\n")?;
    fs::write(dir.join("  中文文件 .txt  "), "unicode and edge spaces\n")?;
    fs::write(dir.join("binary.bin"), [0, 1, 2, 13, 10, 127, 255])?;
    let log_dir = workspace.join(CASE_USER).join(AB_LOG_CID).join(".logs");
    fs::create_dir_all(&log_dir)?;
    fs::write(
        log_dir.join("ab.log"),
        "first line\n\nsecond line\nthird line\nfourth line\n",
    )?;
    fs::write(
        workspace.join("file-server-ab-outside-secret.txt"),
        "must remain outside the selected session root\n",
    )?;
    let preserved_agents = workspace
        .join(CASE_USER)
        .join(AB_IMPORT_CID)
        .join(".agents");
    fs::create_dir_all(&preserved_agents)?;
    fs::write(preserved_agents.join("keep.txt"), "preserved agent data")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        symlink("sub/nested/hello.txt", dir.join("inside-link.txt"))?;
        symlink(
            "../../file-server-ab-outside-secret.txt",
            dir.join("outside-link.txt"),
        )?;
        // 目录链接 → 会话根之外的工作区根 (TS e822516 escapesRoot 对照:
        // relativePath 指进/静态读取经此链接必须被拒)
        symlink("../..", dir.join("outside-dir"))?;
    }
    Ok(())
}
