use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: cargo run -p workspace-manifest --example validate -- <workspace>")?;
    let content = std::fs::read_to_string(workspace.join("workspace.manifest.toml"))?;
    let manifest = workspace_manifest::parse_workspace(&content)?;
    let projects = workspace_manifest::discover_projects(&workspace)?;
    println!(
        "workspace={} enabled_services={}",
        manifest.workspace.name,
        projects
            .iter()
            .filter(|project| project.manifest.project.enabled)
            .count()
    );
    Ok(())
}
