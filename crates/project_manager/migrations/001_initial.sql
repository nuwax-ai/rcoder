-- Create projects table
CREATE TABLE projects (
    id BLOB PRIMARY KEY,
    name TEXT NOT NULL,
    path TEXT NOT NULL UNIQUE,
    description TEXT,
    created_at DATETIME NOT NULL,
    updated_at DATETIME NOT NULL
);

-- Create project_sessions table to track ACP sessions
CREATE TABLE project_sessions (
    id BLOB PRIMARY KEY,
    project_id BLOB NOT NULL,
    acp_session_id TEXT NOT NULL,
    created_at DATETIME NOT NULL,
    last_activity DATETIME NOT NULL,
    status TEXT NOT NULL
);

-- Create file_changes table to track file modifications
CREATE TABLE file_changes (
    id BLOB PRIMARY KEY,
    project_id BLOB NOT NULL,
    file_path TEXT NOT NULL,
    change_type TEXT NOT NULL,
    content TEXT,
    created_at DATETIME NOT NULL
);

-- Create prompts table to store user prompts and responses
CREATE TABLE prompts (
    id BLOB PRIMARY KEY,
    project_id BLOB NOT NULL,
    prompt TEXT NOT NULL,
    response TEXT,
    status TEXT NOT NULL,
    created_at DATETIME NOT NULL,
    completed_at DATETIME
);

-- Create indexes for better performance
CREATE INDEX idx_projects_name ON projects(name);
CREATE INDEX idx_projects_created_at ON projects(created_at);
CREATE INDEX idx_project_sessions_project_id ON project_sessions(project_id);
CREATE INDEX idx_project_sessions_status ON project_sessions(status);
CREATE INDEX idx_file_changes_project_id ON file_changes(project_id);
CREATE INDEX idx_file_changes_created_at ON file_changes(created_at);
CREATE INDEX idx_prompts_project_id ON prompts(project_id);
CREATE INDEX idx_prompts_status ON prompts(status);
CREATE INDEX idx_prompts_created_at ON prompts(created_at);