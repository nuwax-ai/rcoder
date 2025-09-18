use anyhow::{Context, Result};
use serde_json;
use tracing::{debug, warn};

use crate::types::{V0FileData, V0FileEntry, V0ParseResult, V0ParseError};
use crate::utils::calculate_hash;

impl V0FileData {
    /// 从 JSON 字符串创建 V0FileData
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json)
            .context("Failed to parse V0 file JSON")
    }

    /// 解析源内容为 V0ParseResult
    pub fn parse_source(&self) -> Result<V0ParseResult> {
        let mut files = Vec::new();
        let sections: Vec<&str> = self.source.split("[V0_FILE]").collect();

        debug!("Found {} V0 file sections", sections.len() - 1);

        for section in sections.iter().skip(1) {
            match self.parse_file_section(section) {
                Ok(file_entry) => files.push(file_entry),
                Err(e) => {
                    warn!("Failed to parse V0 file section: {}", e);
                    // For debugging, let's log the first 100 chars of the failed section
                    debug!("Failed section preview: {}", &section[..100.min(section.len())]);
                }
            }
        }

        Ok(V0ParseResult {
            files,
            block_id: self.block_id.clone(),
        })
    }

    /// 解析单个文件段落
    fn parse_file_section(&self, section: &str) -> Result<V0FileEntry> {
        let mut lines = section.lines();
        let header = lines.next().ok_or_else(|| V0ParseError::InvalidFormat("Missing header".to_string()))?;

        debug!("Parsing header: {}", header);

        let mut file_type = String::new();
        let mut file_path = std::path::PathBuf::new();
        let mut is_merged = false;
        let mut is_edit = false;
        let mut is_quick_edit = false;
        let mut url = None;

        // Parse header attributes - handle the format: filetype:file="path" isMerged="true" url="..."

        // First, extract the file type from the beginning
        if let Some(colon_pos) = header.find(':') {
            file_type = header[..colon_pos].to_string();
            let rest = &header[colon_pos + 1..];

            debug!("File type: {}, Rest: {}", file_type, rest);

            // Now parse the rest using a more robust approach
            let mut current = rest;

            // Find file="..."
            if let Some(file_start) = current.find("file=\"") {
                current = &current[file_start + 6..];
                if let Some(file_end) = current.find('"') {
                    let path_str = &current[..file_end];
                    file_path = std::path::PathBuf::from(path_str);
                    current = &current[file_end + 1..];
                    debug!("Found file path: {:?}", file_path);
                }
            }

            // Find isMerged="..."
            if let Some(merged_start) = current.find("isMerged=\"") {
                current = &current[merged_start + 10..];
                if let Some(merged_end) = current.find('"') {
                    is_merged = &current[..merged_end] == "true";
                    current = &current[merged_end + 1..];
                    debug!("Found isMerged: {}", is_merged);
                }
            }

            // Find isEdit="..."
            if let Some(edit_start) = current.find("isEdit=\"") {
                current = &current[edit_start + 8..];
                if let Some(edit_end) = current.find('"') {
                    is_edit = &current[..edit_end] == "true";
                    current = &current[edit_end + 1..];
                    debug!("Found isEdit: {}", is_edit);
                }
            }

            // Find isQuickEdit="..."
            if let Some(quick_edit_start) = current.find("isQuickEdit=\"") {
                current = &current[quick_edit_start + 12..];
                if let Some(quick_edit_end) = current.find('"') {
                    is_quick_edit = &current[..quick_edit_end] == "true";
                    current = &current[quick_edit_end + 1..];
                    debug!("Found isQuickEdit: {}", is_quick_edit);
                }
            }

            // Find url="..."
            if let Some(url_start) = current.find("url=\"") {
                current = &current[url_start + 5..];
                if let Some(url_end) = current.find('"') {
                    url = Some(current[..url_end].to_string());
                    debug!("Found url: {:?}", url);
                }
            }
        }

        if file_path.as_os_str().is_empty() {
            return Err(V0ParseError::MissingAttribute("file path".to_string()).into());
        }

        // Collect content (all lines after the header)
        let content: String = lines.collect::<Vec<_>>().join("\n");

        // Calculate hash
        let hash = calculate_hash(&content);

        Ok(V0FileEntry {
            file_type,
            file_path,
            is_merged,
            is_edit,
            is_quick_edit,
            url,
            content,
            hash,
        })
    }
}