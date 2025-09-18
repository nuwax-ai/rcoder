use anyhow::{Context, Result};
use shared_types::Project;
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{debug, info};
use uuid::Uuid;

pub struct ProjectBuilder {
    base_projects_dir: PathBuf,
}

impl ProjectBuilder {
    pub fn new() -> Self {
        Self {
            base_projects_dir: PathBuf::from("./projects"),
        }
    }

    pub async fn create_project_structure(
        &self,
        project_name: &str,
        description: Option<&str>,
        template: Option<&str>,
        base_path: Option<&PathBuf>,
    ) -> Result<PathBuf> {
        info!("Creating project structure for: {}", project_name);

        let project_path = self.get_project_path(project_name, base_path)?;

        // Create the project directory
        fs::create_dir_all(&project_path)
            .await
            .context("Failed to create project directory")?;

        // Create basic project structure
        self.create_basic_structure(&project_path, project_name, template)
            .await?;

        // Create README.md
        self.create_readme(&project_path, project_name, description)
            .await?;

        // Create .gitignore
        self.create_gitignore(&project_path).await?;

        debug!("Project structure created at: {:?}", project_path);

        Ok(project_path)
    }

    fn get_project_path(&self, project_name: &str, base_path: Option<&PathBuf>) -> Result<PathBuf> {
        let base = base_path.unwrap_or(&self.base_projects_dir);
        let sanitized_name = self.sanitize_project_name(project_name);
        Ok(base.join(sanitized_name))
    }

    fn sanitize_project_name(&self, name: &str) -> String {
        name.to_lowercase()
            .replace(|c: char| !c.is_alphanumeric() && c != '_' && c != '-', "_")
    }

    async fn create_basic_structure(
        &self,
        project_path: &Path,
        project_name: &str,
        template: Option<&str>,
    ) -> Result<()> {
        debug!("Creating basic project structure");

        // Create src directory
        fs::create_dir_all(project_path.join("src")).await?;

        // Create basic files based on template
        match template {
            Some("rust") => {
                self.create_rust_project(project_path, project_name).await?;
            }
            Some("node") => {
                self.create_node_project(project_path, project_name).await?;
            }
            Some("python") => {
                self.create_python_project(project_path, project_name).await?;
            }
            _ => {
                self.create_generic_project(project_path, project_name).await?;
            }
        }

        Ok(())
    }

    async fn create_rust_project(&self, project_path: &Path, project_name: &str) -> Result<()> {
        let cargo_toml = format!(
            r#"[package]
name = "{}"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = {{ version = "1.0", features = ["full"] }}
"#,
            project_name
        );

        fs::write(project_path.join("Cargo.toml"), cargo_toml).await?;

        let main_rs = r#"fn main() {
    println!("Hello, {}!");
}"#.replace("{}", project_name);

        fs::write(project_path.join("src/main.rs"), main_rs).await?;

        Ok(())
    }

    async fn create_node_project(&self, project_path: &Path, project_name: &str) -> Result<()> {
        let package_json = format!(
            r#"{{
  "name": "{}",
  "version": "1.0.0",
  "description": "",
  "main": "index.js",
  "scripts": {{
    "start": "node index.js",
    "dev": "nodemon index.js"
  }},
  "dependencies": {{
    "express": "^4.18.0"
  }},
  "devDependencies": {{
    "nodemon": "^3.0.0"
  }}
}}"#,
            project_name
        );

        fs::write(project_path.join("package.json"), package_json).await?;

        let index_js = r#"const express = require('express');
const app = express();
const port = process.env.PORT || 3000;

app.get('/', (req, res) => {
    res.send('Hello from {}!');
});

app.listen(port, () => {
    console.log(`Server running at http://localhost:${port}`);
});"#.replace("{}", project_name);

        fs::write(project_path.join("index.js"), index_js).await?;

        Ok(())
    }

    async fn create_python_project(&self, project_path: &Path, project_name: &str) -> Result<()> {
        let main_py = r#"def main():
    print("Hello from {}!")

if __name__ == "__main__":
    main()"#.replace("{}", project_name);

        fs::write(project_path.join("main.py"), main_py).await?;

        let requirements_txt = r#"# Add your Python dependencies here
requests>=2.31.0"#;

        fs::write(project_path.join("requirements.txt"), requirements_txt).await?;

        Ok(())
    }

    async fn create_generic_project(&self, project_path: &Path, project_name: &str) -> Result<()> {
        let main_file = r#"// {} Project
// This is a generic project template

console.log("Hello from {}!");"#.replace("{}", project_name);

        fs::write(project_path.join("main.js"), main_file).await?;

        Ok(())
    }

    async fn create_readme(
        &self,
        project_path: &Path,
        project_name: &str,
        description: Option<&str>,
    ) -> Result<()> {
        let readme_content = format!(
            r#"# {}

{}

## Getting Started

This project was created using the AI-powered development platform.

## Project Structure

```
├── src/           # Source code
├── README.md      # This file
└── .gitignore     # Git ignore rules
```

## Development

Instructions for development will be added based on your requirements.

## License

This project is open source and available under the MIT License.
"#,
            project_name,
            description.unwrap_or("A project created with AI assistance.")
        );

        fs::write(project_path.join("README.md"), readme_content).await?;

        Ok(())
    }

    async fn create_gitignore(&self, project_path: &Path) -> Result<()> {
        let gitignore_content = r#"# Dependencies
node_modules/
target/
__pycache__/
*.pyc

# Environment variables
.env
.env.local
.env.production

# IDE files
.vscode/
.idea/
*.swp
*.swo

# OS generated files
.DS_Store
.DS_Store?
._*
.Spotlight-V100
.Trashes
ehthumbs.db
Thumbs.db

# Build artifacts
dist/
build/
*.exe
*.dll
*.so
*.dylib

# Logs
logs/
*.log
npm-debug.log*
yarn-debug.log*
yarn-error.log*
"#;

        fs::write(project_path.join(".gitignore"), gitignore_content).await?;

        Ok(())
    }

    pub async fn cleanup_project(&self, project_path: &Path) -> Result<()> {
        info!("Cleaning up project at: {:?}", project_path);

        if project_path.exists() {
            fs::remove_dir_all(project_path).await?;
        }

        Ok(())
    }
}