use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tracing::{debug, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectTemplate {
    pub name: String,
    pub description: String,
    pub language: String,
    pub files: HashMap<String, String>,
    pub dependencies: Option<Vec<String>>,
    pub build_commands: Option<Vec<String>>,
    pub dev_commands: Option<Vec<String>>,
}

pub struct TemplateManager {
    templates: HashMap<String, ProjectTemplate>,
}

impl TemplateManager {
    pub fn new() -> Self {
        let mut manager = Self {
            templates: HashMap::new(),
        };

        manager.load_builtin_templates();
        manager
    }

    fn load_builtin_templates(&mut self) {
        info!("Loading built-in templates");

        // Rust Web API Template
        self.templates.insert("rust-web-api".to_string(), ProjectTemplate {
            name: "Rust Web API".to_string(),
            description: "A REST API using Rust with Axum framework".to_string(),
            language: "Rust".to_string(),
            files: {
                let mut files = HashMap::new();
                files.insert("Cargo.toml".to_string(), r#"[package]
name = "web-api"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { version = "1.0", features = ["full"] }
axum = "0.7"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
tower = "0.4"
tower-http = { version = "0.5", features = ["cors", "trace"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
uuid = { version = "1.0", features = ["v4"] }
anyhow = "1.0"
"#.to_string());

                files.insert("src/main.rs".to_string(), r#"use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tower_http::cors::CorsLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "web_api=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health_check))
        .route("/api/users", get(get_users).post(create_user))
        .layer(CorsLayer::permissive());

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::info!("Listening on {}", addr);

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn root() -> &'static str {
    "Welcome to the Rust Web API!"
}

async fn health_check() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "healthy" }))
}

#[derive(Debug, Serialize, Deserialize)]
struct User {
    id: uuid::Uuid,
    name: String,
    email: String,
}

async fn get_users() -> Json<Vec<User>> {
    let users = vec![
        User {
            id: uuid::Uuid::new_v4(),
            name: "John Doe".to_string(),
            email: "john@example.com".to_string(),
        },
        User {
            id: uuid::Uuid::new_v4(),
            name: "Jane Smith".to_string(),
            email: "jane@example.com".to_string(),
        },
    ];
    Json(users)
}

#[derive(Debug, Deserialize)]
struct CreateUserRequest {
    name: String,
    email: String,
}

async fn create_user(
    Json(payload): Json<CreateUserRequest>,
) -> (StatusCode, Json<User>) {
    let user = User {
        id: uuid::Uuid::new_v4(),
        name: payload.name,
        email: payload.email,
    };
    (StatusCode::CREATED, Json(user))
}
"#.to_string());

                files
            },
            dependencies: Some(vec
!["axum".to_string(), "tokio".to_string(), "serde".to_string(), "serde_json".to_string()]),
            build_commands: Some(vec
!["cargo build --release".to_string()]),
            dev_commands: Some(vec
!["cargo watch -x run".to_string()]),
        });

        // React Frontend Template
        self.templates.insert("react-frontend".to_string(), ProjectTemplate {
            name: "React Frontend".to_string(),
            description: "A modern React application with TypeScript".to_string(),
            language: "TypeScript".to_string(),
            files: {
                let mut files = HashMap::new();
                files.insert("package.json".to_string(), r#"{{
  "name": "react-frontend",
  "version": "1.0.0",
  "private": true,
  "dependencies": {{
    "react": "^18.2.0",
    "react-dom": "^18.2.0",
    "react-router-dom": "^6.8.0",
    "axios": "^1.3.0",
    "@types/react": "^18.0.0",
    "@types/react-dom": "^18.0.0",
    "typescript": "^4.9.0"
  }},
  "scripts": {{
    "start": "react-scripts start",
    "build": "react-scripts build",
    "test": "react-scripts test",
    "eject": "react-scripts eject"
  }},
  "devDependencies": {{
    "react-scripts": "5.0.1",
    "@types/node": "^16.18.0"
  }},
  "browserslist": {{
    "production": [
      ">0.2%",
      "not dead",
      "not op_mini all"
    ],
    "development": [
      "last 1 chrome version",
      "last 1 firefox version",
      "last 1 safari version"
    ]
  }}
}}"#.to_string());

                files.insert("src/App.tsx".to_string(), r#"import React from 'react';
import {{ BrowserRouter as Router, Routes, Route }} from 'react-router-dom';
import {{ HomePage }} from './pages/HomePage';
import {{ AboutPage }} from './pages/AboutPage';
import './App.css';

function App() {{
  return (
    <Router>
      <div className="App">
        <Routes>
          <Route path="/" element={{<HomePage />}} />
          <Route path="/about" element={{<AboutPage />}} />
        </Routes>
      </div>
    </Router>
  );
}}

export default App;
"#.to_string());

                files
            },
            dependencies: Some(vec
!["react".to_string(), "react-dom".to_string(), "react-router-dom".to_string()]),
            build_commands: Some(vec
!["npm run build".to_string()]),
            dev_commands: Some(vec
!["npm start".to_string()]),
        });

        // Python CLI Template
        self.templates.insert("python-cli".to_string(), ProjectTemplate {
            name: "Python CLI Tool".to_string(),
            description: "A command-line interface tool using Python and Click".to_string(),
            language: "Python".to_string(),
            files: {
                let mut files = HashMap::new();
                files.insert("requirements.txt".to_string(), r#"click>=8.0.0
rich>=13.0.0
pydantic>=2.0.0"#.to_string());

                files.insert("main.py".to_string(), r#"#!/usr/bin/env python3
"""
Python CLI Tool
A modern command-line interface tool.
"""

import click
from rich.console import Console
from rich.table import Table
from typing import Optional

console = Console()

@click.group()
@click.version_option(version="1.0.0")
def cli():
    """A modern CLI tool built with Python and Click."""
    pass

@cli.command()
@click.option('--name', prompt='Your name', help='Name to greet.')
@click.option('--count', default=1, help='Number of greetings.')
def hello(name: str, count: int):
    """Simple program that greets NAME for a total of COUNT times."""
    for _ in range(count):
        console.print(f"Hello, [bold green]{name}[/bold green]!")

@cli.command()
@click.option('--format', type=click.Choice(['table', 'json']), default='table', help='Output format.')
def list(format: str):
    """List example items in different formats."""
    items = [
        {"id": 1, "name": "Item 1", "status": "Active"},
        {"id": 2, "name": "Item 2", "status": "Inactive"},
        {"id": 3, "name": "Item 3", "status": "Active"},
    ]

    if format == 'table':
        table = Table(title="Items")
        table.add_column("ID", style="cyan")
        table.add_column("Name", style="magenta")
        table.add_column("Status", style="green")

        for item in items:
            style = "bold" if item["status"] == "Active" else "dim"
            table.add_row(str(item["id"]), item["name"], item["status"], style=style)

        console.print(table)
    else:
        import json
        console.print_json(data=items)

@cli.command()
@click.argument('path', type=click.Path(exists=True))
def analyze(path: str):
    """Analyze the given file or directory."""
    console.print(f"Analyzing: [bold blue]{path}[/bold blue]")
    # Add your analysis logic here
    console.print("Analysis complete!")

if __name__ == '__main__':
    cli()
"#.to_string());

                files
            },
            dependencies: Some(vec
!["click".to_string(), "rich".to_string(), "pydantic".to_string()]),
            build_commands: Some(vec
!["pip install -r requirements.txt".to_string()]),
            dev_commands: Some(vec
!["python main.py --help".to_string()]),
        });

        info!("Loaded {} built-in templates", self.templates.len());
    }

    pub fn get_template(&self, name: &str) -> Option<&ProjectTemplate> {
        self.templates.get(name)
    }

    pub fn list_templates(&self) -> Vec<&ProjectTemplate> {
        self.templates.values().collect()
    }

    pub fn add_custom_template(&mut self, template: ProjectTemplate) {
        let name = template.name.clone();
        self.templates.insert(name, template);
    }

    pub async fn apply_template(
        &self,
        template_name: &str,
        project_path: &Path,
    ) -> Result<()> {
        info!("Applying template: {}", template_name);

        let template = self.get_template(template_name)
            .ok_or_else(|| anyhow::anyhow!("Template '{}' not found", template_name))?;

        for (file_path, content) in &template.files {
            let full_path = project_path.join(file_path);

            // Create parent directories if they don't exist
            if let Some(parent) = full_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            tokio::fs::write(&full_path, content).await
                .with_context(|| format!("Failed to write file: {}", file_path))?;
        }

        debug!("Applied template files for: {}", template_name);
        Ok(())
    }

    pub fn get_template_info(&self, template_name: &str) -> Option<&ProjectTemplate> {
        self.get_template(template_name)
    }

    pub fn validate_template(&self, template: &ProjectTemplate) -> Result<()> {
        if template.name.is_empty() {
            return Err(anyhow::anyhow!("Template name cannot be empty"));
        }

        if template.files.is_empty() {
            return Err(anyhow::anyhow!("Template must have at least one file"));
        }

        // Validate that all required files are present
        let required_files = match template.language.as_str() {
            "Rust" => vec!["Cargo.toml", "src/main.rs"],
            "TypeScript" | "JavaScript" => vec!["package.json"],
            "Python" => vec!["requirements.txt", "main.py"],
            _ => vec![],
        };

        for required_file in required_files {
            if !template.files.contains_key(required_file) {
                return Err(anyhow::anyhow!(
                    "Template for {} must include {}",
                    template.language,
                    required_file
                ));
            }
        }

        Ok(())
    }
}