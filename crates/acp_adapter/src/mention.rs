use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    str::FromStr,
};
use url::Url;

/// 统一资源标识系统 - 参考Zed的MentionUri设计
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum ResourceUri {
    /// 文件资源
    File {
        abs_path: PathBuf,
    },
    /// 目录资源
    Directory {
        abs_path: PathBuf,
    },
    /// 代码符号
    Symbol {
        abs_path: PathBuf,
        name: String,
        line_range: RangeInclusive<u32>,
    },
    /// 代码选择区域
    Selection {
        abs_path: Option<PathBuf>,
        line_range: RangeInclusive<u32>,
    },
    /// 会话线程
    Thread {
        id: String,
        name: String,
    },
    /// 文本会话线程（基于文件的会话）
    TextThread {
        path: PathBuf,
        name: String,
    },
    /// 工具调用
    ToolCall {
        id: String,
        tool_name: String,
        status: String,
    },
    /// Terminal会话
    Terminal {
        id: String,
        command: String,
        status: String,
    },
    /// 网络资源
    Web {
        url: Url,
    },
    /// 粘贴的图片
    PastedImage {
        id: String,
    },
    /// 内存中的缓冲区
    Buffer {
        id: String,
        name: String,
        language: Option<String>,
    },
    /// 规则或模板
    Rule {
        id: String,
        name: String,
        category: Option<String>,
    },
    /// Git相关资源
    Git {
        repo_path: PathBuf,
        resource_type: GitResourceType,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum GitResourceType {
    Commit { hash: String },
    Branch { name: String },
    Tag { name: String },
    Diff { from: String, to: String },
}

impl ResourceUri {
    /// 从字符串解析ResourceUri
    pub fn parse(input: &str) -> Result<Self> {
        fn parse_line_range(fragment: &str) -> Result<RangeInclusive<u32>> {
            let range = fragment
                .strip_prefix("L")
                .context("Line range must start with \"L\"")?;
            let (start, end) = range
                .split_once(":")
                .context("Line range must use colon as separator")?;
            let range = start
                .parse::<u32>()
                .context("Parsing line range start")?
                .checked_sub(1)
                .context("Line numbers should be 1-based")?
                ..=end
                    .parse::<u32>()
                    .context("Parsing line range end")?
                    .checked_sub(1)
                    .context("Line numbers should be 1-based")?;
            Ok(range)
        }

        let url = url::Url::parse(input)?;
        let path = url.path();
        
        match url.scheme() {
            "file" => {
                let abs_path = url.to_file_path().ok().context("Extracting file path")?;
                if let Some(fragment) = url.fragment() {
                    let line_range = parse_line_range(fragment)?;
                    if let Some(symbol_name) = single_query_param(&url, "symbol")? {
                        Ok(Self::Symbol {
                            name: symbol_name,
                            abs_path,
                            line_range,
                        })
                    } else {
                        Ok(Self::Selection {
                            abs_path: Some(abs_path),
                            line_range,
                        })
                    }
                } else if input.ends_with("/") {
                    Ok(Self::Directory { abs_path })
                } else {
                    Ok(Self::File { abs_path })
                }
            }
            "rcoder" => {
                // 我们自己的scheme
                if let Some(thread_id) = path.strip_prefix("/thread/") {
                    let name = single_query_param(&url, "name")?.context("Missing thread name")?;
                    Ok(Self::Thread {
                        id: thread_id.to_string(),
                        name,
                    })
                } else if let Some(path_str) = path.strip_prefix("/text-thread/") {
                    let name = single_query_param(&url, "name")?.context("Missing thread name")?;
                    Ok(Self::TextThread {
                        path: path_str.into(),
                        name,
                    })
                } else if let Some(tool_call_id) = path.strip_prefix("/tool-call/") {
                    let tool_name = single_query_param(&url, "tool_name")?.context("Missing tool name")?;
                    let status = single_query_param(&url, "status")?.unwrap_or_else(|| "unknown".to_string());
                    Ok(Self::ToolCall {
                        id: tool_call_id.to_string(),
                        tool_name,
                        status,
                    })
                } else if let Some(terminal_id) = path.strip_prefix("/terminal/") {
                    let command = single_query_param(&url, "command")?.context("Missing command")?;
                    let status = single_query_param(&url, "status")?.unwrap_or_else(|| "unknown".to_string());
                    Ok(Self::Terminal {
                        id: terminal_id.to_string(),
                        command,
                        status,
                    })
                } else if let Some(rule_id) = path.strip_prefix("/rule/") {
                    let name = single_query_param(&url, "name")?.context("Missing rule name")?;
                    let category = single_query_param(&url, "category")?;
                    Ok(Self::Rule {
                        id: rule_id.to_string(),
                        name,
                        category,
                    })
                } else if let Some(buffer_id) = path.strip_prefix("/buffer/") {
                    let name = single_query_param(&url, "name")?.context("Missing buffer name")?;
                    let language = single_query_param(&url, "language")?;
                    Ok(Self::Buffer {
                        id: buffer_id.to_string(),
                        name,
                        language,
                    })
                } else if path.starts_with("/pasted-image/") {
                    let image_id = path.strip_prefix("/pasted-image/").unwrap_or("unknown");
                    Ok(Self::PastedImage {
                        id: image_id.to_string(),
                    })
                } else if path.starts_with("/untitled-buffer") {
                    let fragment = url
                        .fragment()
                        .context("Missing fragment for untitled buffer selection")?;
                    let line_range = parse_line_range(fragment)?;
                    Ok(Self::Selection {
                        abs_path: None,
                        line_range,
                    })
                } else if let Some(repo_path_str) = path.strip_prefix("/git/") {
                    let repo_path = PathBuf::from(repo_path_str);
                    let resource_type = if let Some(commit_hash) = single_query_param(&url, "commit")? {
                        GitResourceType::Commit { hash: commit_hash }
                    } else if let Some(branch_name) = single_query_param(&url, "branch")? {
                        GitResourceType::Branch { name: branch_name }
                    } else if let Some(tag_name) = single_query_param(&url, "tag")? {
                        GitResourceType::Tag { name: tag_name }
                    } else if let Some(from) = single_query_param(&url, "from")? {
                        let to = single_query_param(&url, "to")?.context("Missing 'to' parameter for diff")?;
                        GitResourceType::Diff { from, to }
                    } else {
                        bail!("Missing git resource type parameters");
                    };
                    Ok(Self::Git {
                        repo_path,
                        resource_type,
                    })
                } else {
                    bail!("invalid rcoder url: {:?}", input);
                }
            }
            "http" | "https" => Ok(ResourceUri::Web { url }),
            other => bail!("unrecognized scheme {:?}", other),
        }
    }

    /// 获取资源的显示名称
    pub fn name(&self) -> String {
        match self {
            ResourceUri::File { abs_path } | ResourceUri::Directory { abs_path } => {
                abs_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }
            ResourceUri::Symbol { name, .. } => name.clone(),
            ResourceUri::Selection {
                abs_path: path,
                line_range,
                ..
            } => selection_name(path.as_deref(), line_range),
            ResourceUri::Thread { name, .. } => name.clone(),
            ResourceUri::TextThread { name, .. } => name.clone(),
            ResourceUri::ToolCall { tool_name, id, .. } => format!("{} ({})", tool_name, &id[..8]),
            ResourceUri::Terminal { command, id, .. } => format!("{} ({})", command, &id[..8]),
            ResourceUri::Web { url } => url.to_string(),
            ResourceUri::PastedImage { id } => format!("Image ({})", &id[..8]),
            ResourceUri::Buffer { name, .. } => name.clone(),
            ResourceUri::Rule { name, .. } => name.clone(),
            ResourceUri::Git { resource_type, .. } => match resource_type {
                GitResourceType::Commit { hash } => format!("Commit {}", &hash[..8]),
                GitResourceType::Branch { name } => format!("Branch {}", name),
                GitResourceType::Tag { name } => format!("Tag {}", name),
                GitResourceType::Diff { from, to } => format!("Diff {}..{}", &from[..8], &to[..8]),
            },
        }
    }

    /// 获取资源图标名称（用于UI显示）
    pub fn icon_name(&self) -> &'static str {
        match self {
            ResourceUri::File { .. } => "file",
            ResourceUri::Directory { .. } => "folder",
            ResourceUri::Symbol { .. } => "code",
            ResourceUri::Selection { .. } => "selection",
            ResourceUri::Thread { .. } => "thread",
            ResourceUri::TextThread { .. } => "thread",
            ResourceUri::ToolCall { .. } => "tool",
            ResourceUri::Terminal { .. } => "terminal",
            ResourceUri::Web { .. } => "web",
            ResourceUri::PastedImage { .. } => "image",
            ResourceUri::Buffer { .. } => "buffer",
            ResourceUri::Rule { .. } => "rule",
            ResourceUri::Git { resource_type, .. } => match resource_type {
                GitResourceType::Commit { .. } => "git-commit",
                GitResourceType::Branch { .. } => "git-branch",
                GitResourceType::Tag { .. } => "git-tag",
                GitResourceType::Diff { .. } => "git-diff",
            },
        }
    }

    /// 转换为链接格式的引用
    pub fn as_link<'a>(&'a self) -> ResourceLink<'a> {
        ResourceLink(self)
    }

    /// 转换为URI字符串
    pub fn to_uri(&self) -> Url {
        match self {
            ResourceUri::File { abs_path } => {
                Url::from_file_path(abs_path).expect("path should be absolute")
            }
            ResourceUri::Directory { abs_path } => {
                Url::from_directory_path(abs_path).expect("path should be absolute")
            }
            ResourceUri::Symbol {
                abs_path,
                name,
                line_range,
            } => {
                let mut url =
                    Url::from_file_path(abs_path).expect("path should be absolute");
                url.query_pairs_mut().append_pair("symbol", name);
                url.set_fragment(Some(&format!(
                    "L{}:{}",
                    line_range.start() + 1,
                    line_range.end() + 1
                )));
                url
            }
            ResourceUri::Selection {
                abs_path: path,
                line_range,
            } => {
                let mut url = if let Some(path) = path {
                    Url::from_file_path(path).expect("path should be absolute")
                } else {
                    let mut url = Url::parse("rcoder:///").unwrap();
                    url.set_path("/untitled-buffer");
                    url
                };
                url.set_fragment(Some(&format!(
                    "L{}:{}",
                    line_range.start() + 1,
                    line_range.end() + 1
                )));
                url
            }
            ResourceUri::Thread { name, id } => {
                let mut url = Url::parse("rcoder:///").unwrap();
                url.set_path(&format!("/thread/{}", id));
                url.query_pairs_mut().append_pair("name", name);
                url
            }
            ResourceUri::TextThread { path, name } => {
                let mut url = Url::parse("rcoder:///").unwrap();
                url.set_path(&format!(
                    "/text-thread/{}",
                    path.to_string_lossy().trim_start_matches('/')
                ));
                url.query_pairs_mut().append_pair("name", name);
                url
            }
            ResourceUri::ToolCall { id, tool_name, status } => {
                let mut url = Url::parse("rcoder:///").unwrap();
                url.set_path(&format!("/tool-call/{}", id));
                url.query_pairs_mut().append_pair("tool_name", tool_name);
                url.query_pairs_mut().append_pair("status", status);
                url
            }
            ResourceUri::Terminal { id, command, status } => {
                let mut url = Url::parse("rcoder:///").unwrap();
                url.set_path(&format!("/terminal/{}", id));
                url.query_pairs_mut().append_pair("command", command);
                url.query_pairs_mut().append_pair("status", status);
                url
            }
            ResourceUri::Web { url } => url.clone(),
            ResourceUri::PastedImage { id } => {
                let mut url = Url::parse("rcoder:///").unwrap();
                url.set_path(&format!("/pasted-image/{}", id));
                url
            }
            ResourceUri::Buffer { id, name, language } => {
                let mut url = Url::parse("rcoder:///").unwrap();
                url.set_path(&format!("/buffer/{}", id));
                url.query_pairs_mut().append_pair("name", name);
                if let Some(lang) = language {
                    url.query_pairs_mut().append_pair("language", lang);
                }
                url
            }
            ResourceUri::Rule { id, name, category } => {
                let mut url = Url::parse("rcoder:///").unwrap();
                url.set_path(&format!("/rule/{}", id));
                url.query_pairs_mut().append_pair("name", name);
                if let Some(cat) = category {
                    url.query_pairs_mut().append_pair("category", cat);
                }
                url
            }
            ResourceUri::Git { repo_path, resource_type } => {
                let mut url = Url::parse("rcoder:///").unwrap();
                url.set_path(&format!("/git/{}", repo_path.to_string_lossy()));
                match resource_type {
                    GitResourceType::Commit { hash } => {
                        url.query_pairs_mut().append_pair("commit", hash);
                    }
                    GitResourceType::Branch { name } => {
                        url.query_pairs_mut().append_pair("branch", name);
                    }
                    GitResourceType::Tag { name } => {
                        url.query_pairs_mut().append_pair("tag", name);
                    }
                    GitResourceType::Diff { from, to } => {
                        url.query_pairs_mut().append_pair("from", from);
                        url.query_pairs_mut().append_pair("to", to);
                    }
                }
                url
            }
        }
    }
}

impl FromStr for ResourceUri {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Self::parse(s)
    }
}

/// 链接格式的资源引用
pub struct ResourceLink<'a>(&'a ResourceUri);

impl fmt::Display for ResourceLink<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[@{}]({})", self.0.name(), self.0.to_uri())
    }
}

/// 辅助函数：获取单个查询参数
fn single_query_param(url: &Url, name: &'static str) -> Result<Option<String>> {
    let pairs = url.query_pairs().collect::<Vec<_>>();
    let matching_pairs: Vec<_> = pairs.iter().filter(|(k, _)| k == name).collect();
    
    match matching_pairs.len() {
        0 => Ok(None),
        1 => Ok(Some(matching_pairs[0].1.to_string())),
        _ => bail!("multiple values for query parameter '{}'", name),
    }
}

/// 生成选择区域的显示名称
pub fn selection_name(path: Option<&Path>, line_range: &RangeInclusive<u32>) -> String {
    format!(
        "{} ({}:{})",
        path.and_then(|path| path.file_name())
            .unwrap_or("Untitled".as_ref())
            .to_string_lossy(),
        *line_range.start() + 1,
        *line_range.end() + 1
    )
}

/// 资源URI构建器
pub struct ResourceUriBuilder;

impl ResourceUriBuilder {
    /// 创建文件URI
    pub fn file<P: AsRef<Path>>(path: P) -> ResourceUri {
        ResourceUri::File {
            abs_path: path.as_ref().to_path_buf(),
        }
    }

    /// 创建目录URI
    pub fn directory<P: AsRef<Path>>(path: P) -> ResourceUri {
        ResourceUri::Directory {
            abs_path: path.as_ref().to_path_buf(),
        }
    }

    /// 创建符号URI
    pub fn symbol<P: AsRef<Path>>(
        path: P,
        name: String,
        line_range: RangeInclusive<u32>,
    ) -> ResourceUri {
        ResourceUri::Symbol {
            abs_path: path.as_ref().to_path_buf(),
            name,
            line_range,
        }
    }

    /// 创建选择URI
    pub fn selection<P: AsRef<Path>>(
        path: Option<P>,
        line_range: RangeInclusive<u32>,
    ) -> ResourceUri {
        ResourceUri::Selection {
            abs_path: path.map(|p| p.as_ref().to_path_buf()),
            line_range,
        }
    }

    /// 创建线程URI
    pub fn thread(id: String, name: String) -> ResourceUri {
        ResourceUri::Thread { id, name }
    }

    /// 创建工具调用URI
    pub fn tool_call(id: String, tool_name: String, status: String) -> ResourceUri {
        ResourceUri::ToolCall { id, tool_name, status }
    }

    /// 创建Terminal URI
    pub fn terminal(id: String, command: String, status: String) -> ResourceUri {
        ResourceUri::Terminal { id, command, status }
    }

    /// 创建Web URI
    pub fn web(url: Url) -> ResourceUri {
        ResourceUri::Web { url }
    }

    /// 创建Git提交URI
    pub fn git_commit<P: AsRef<Path>>(repo_path: P, hash: String) -> ResourceUri {
        ResourceUri::Git {
            repo_path: repo_path.as_ref().to_path_buf(),
            resource_type: GitResourceType::Commit { hash },
        }
    }

    /// 创建Git分支URI
    pub fn git_branch<P: AsRef<Path>>(repo_path: P, name: String) -> ResourceUri {
        ResourceUri::Git {
            repo_path: repo_path.as_ref().to_path_buf(),
            resource_type: GitResourceType::Branch { name },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_file_uri() {
        let file_uri = "file:///path/to/file.rs";
        let parsed = ResourceUri::parse(file_uri).unwrap();
        match &parsed {
            ResourceUri::File { abs_path } => {
                assert_eq!(abs_path.to_str().unwrap(), "/path/to/file.rs");
            }
            _ => panic!("Expected File variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), file_uri);
    }

    #[test]
    fn test_parse_symbol_uri() {
        let symbol_uri = "file:///path/to/file.rs?symbol=MySymbol#L10:20";
        let parsed = ResourceUri::parse(symbol_uri).unwrap();
        match &parsed {
            ResourceUri::Symbol {
                abs_path: path,
                name,
                line_range,
            } => {
                assert_eq!(path.to_str().unwrap(), "/path/to/file.rs");
                assert_eq!(name, "MySymbol");
                assert_eq!(line_range.start(), &9);
                assert_eq!(line_range.end(), &19);
            }
            _ => panic!("Expected Symbol variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), symbol_uri);
    }

    #[test]
    fn test_parse_thread_uri() {
        let thread_uri = "rcoder:///thread/session123?name=Thread+name";
        let parsed = ResourceUri::parse(thread_uri).unwrap();
        match &parsed {
            ResourceUri::Thread {
                id: thread_id,
                name,
            } => {
                assert_eq!(thread_id, "session123");
                assert_eq!(name, "Thread name");
            }
            _ => panic!("Expected Thread variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), thread_uri);
    }

    #[test]
    fn test_parse_tool_call_uri() {
        let tool_uri = "rcoder:///tool-call/abc123?tool_name=run_terminal&status=running";
        let parsed = ResourceUri::parse(tool_uri).unwrap();
        match &parsed {
            ResourceUri::ToolCall { id, tool_name, status } => {
                assert_eq!(id, "abc123");
                assert_eq!(tool_name, "run_terminal");
                assert_eq!(status, "running");
            }
            _ => panic!("Expected ToolCall variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), tool_uri);
    }

    #[test]
    fn test_parse_git_commit_uri() {
        let git_uri = "rcoder:///git/my-repo?commit=abc123def456";
        let parsed = ResourceUri::parse(git_uri).unwrap();
        match &parsed {
            ResourceUri::Git { repo_path, resource_type } => {
                assert_eq!(repo_path.to_str().unwrap(), "my-repo");
                match resource_type {
                    GitResourceType::Commit { hash } => {
                        assert_eq!(hash, "abc123def456");
                    }
                    _ => panic!("Expected Commit variant"),
                }
            }
            _ => panic!("Expected Git variant"),
        }
        assert_eq!(parsed.to_uri().to_string(), git_uri);
    }

    #[test]
    fn test_builder_methods() {
        let file_uri = ResourceUriBuilder::file("/path/to/file.rs");
        assert!(matches!(file_uri, ResourceUri::File { .. }));

        let symbol_uri = ResourceUriBuilder::symbol("/path/to/file.rs", "MySymbol".to_string(), 10..=20);
        assert!(matches!(symbol_uri, ResourceUri::Symbol { .. }));

        let thread_uri = ResourceUriBuilder::thread("session123".to_string(), "My Thread".to_string());
        assert!(matches!(thread_uri, ResourceUri::Thread { .. }));
    }

    #[test]
    fn test_icon_names() {
        let file_uri = ResourceUriBuilder::file("/test.rs");
        assert_eq!(file_uri.icon_name(), "file");

        let dir_uri = ResourceUriBuilder::directory("/test");
        assert_eq!(dir_uri.icon_name(), "folder");

        let terminal_uri = ResourceUriBuilder::terminal("term1".to_string(), "ls".to_string(), "running".to_string());
        assert_eq!(terminal_uri.icon_name(), "terminal");
    }

    #[test]
    fn test_display_names() {
        let file_uri = ResourceUriBuilder::file("/path/to/test.rs");
        assert_eq!(file_uri.name(), "test.rs");

        let symbol_uri = ResourceUriBuilder::symbol("/test.rs", "MyFunction".to_string(), 10..=20);
        assert_eq!(symbol_uri.name(), "MyFunction");

        let git_commit_uri = ResourceUriBuilder::git_commit("/repo", "abc123def456".to_string());
        assert_eq!(git_commit_uri.name(), "Commit abc123de");
    }

    #[test]
    fn test_link_formatting() {
        let file_uri = ResourceUriBuilder::file("/test.rs");
        let link = file_uri.as_link();
        let link_str = format!("{}", link);
        assert!(link_str.starts_with("[@test.rs](file:///"));
    }

    #[test]
    fn test_invalid_schemes() {
        assert!(ResourceUri::parse("ftp://example.com").is_err());
        assert!(ResourceUri::parse("ssh://example.com").is_err());
        assert!(ResourceUri::parse("unknown://example.com").is_err());
    }
}