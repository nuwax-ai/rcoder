//! Small real build fixtures, independent of external language packages and LLMs.
use super::{Env, report::JsonlReporter};
use std::io::Write;

pub async fn failing_build(env: &Env, report: &JsonlReporter, app: &str) {
    let workspace = env
        .http
        .post(format!("{}/api/v1/userapp/workspace", env.rcoder))
        .json(&serde_json::json!({"app_id": app, "user_id": "e2e-user"}))
        .send()
        .await
        .expect("create test workspace");
    let body: serde_json::Value = workspace.json().await.expect("workspace envelope");
    report.assert_hard(
        "fixture workspace created",
        body["code"] == "0000",
        body["code"].to_string(),
    );
    let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let files = [
        (
            "workspace.manifest.toml",
            "schema_version = 1\n[workspace]\nname = \"failure-fixture\"\n",
        ),
        (
            "web/project.manifest.toml",
            "schema_version = 1\n[project]\nservice_id = \"web\"\nname = \"Failure Fixture\"\ntype = \"static\"\n[build]\ncommand = [\"sh\", \"-c\", \"exit 17\"]\nartifact = \"dist\"\n[proxy]\npath = \"/web\"\nstrip_prefix = true\n",
        ),
        (
            "web/dist/index.html",
            "<!doctype html><title>build failure fixture</title>",
        ),
    ];
    for (name, content) in files {
        archive
            .start_file(name, zip::write::SimpleFileOptions::default())
            .expect("zip entry");
        archive.write_all(content.as_bytes()).expect("zip content");
    }
    let part = reqwest::multipart::Part::bytes(archive.finish().expect("zip finish").into_inner())
        .file_name("failure-fixture.zip");
    let form = reqwest::multipart::Form::new()
        .text("app_id", app.to_owned())
        .text("user_id", "e2e-user")
        .text("enable_git", "false")
        .part("file", part);
    let response = env
        .http
        .post(format!(
            "{}/api/v1/userapp/init-project-template",
            env.rcoder
        ))
        .header("X-App-Id", app)
        .multipart(form)
        .send()
        .await
        .expect("import fixture");
    let status = response.status();
    let body: serde_json::Value = response.json().await.expect("fixture response");
    report.assert_hard(
        "real failing build fixture imported",
        status.is_success()
            && body["success"] == true
            && body["workspace_root"]
                .as_str()
                .is_some_and(|path| path.ends_with(app)),
        body.to_string(),
    );
}
