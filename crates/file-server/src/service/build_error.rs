//! 构建错误解析，输出格式与 nuwax `BuildErrorParser` 保持一致。

use std::path::Path;

use regex::Regex;

struct FileInfo {
    path: String,
    line: usize,
    column: usize,
}

struct ErrorDetails {
    kind: String,
    message: String,
}

struct Suggestion {
    message: &'static str,
    example: Option<(&'static str, &'static str)>,
}

pub fn parse(error_message: &str) -> String {
    let file_info = extract_file_info(error_message);
    let details = extract_error_details(error_message);
    let suggestions = error_suggestions(error_message);
    render(&details, file_info.as_ref(), &suggestions)
}

fn extract_file_info(message: &str) -> Option<FileInfo> {
    let captures = captures(r"file:\s*([^\n]+):(\d+):(\d+)", message)?;
    Some(FileInfo {
        path: captures.get(1)?.as_str().trim().to_string(),
        line: captures.get(2)?.as_str().parse().ok()?,
        column: captures.get(3)?.as_str().parse().ok()?,
    })
}

fn extract_error_details(message: &str) -> ErrorDetails {
    let kind = captures(
        r"(Parse error|SyntaxError|TypeError|ReferenceError|Unexpected token)",
        message,
    )
    .and_then(|captures| captures.get(1).map(|value| value.as_str().to_string()))
    .unwrap_or_else(|| "Build error".to_string());
    let description = captures(
        r"(?:Parse error|SyntaxError|TypeError|ReferenceError)[^:]*:\s*([^\n]+)",
        message,
    )
    .and_then(|captures| {
        captures
            .get(1)
            .map(|value| value.as_str().trim().to_string())
    })
    .or_else(|| {
        message
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.contains("file:") && !line.contains("at "))
            .map(ToOwned::to_owned)
    })
    .unwrap_or_else(|| message.to_string());
    ErrorDetails {
        kind,
        message: description,
    }
}

fn error_suggestions(message: &str) -> Vec<Suggestion> {
    let mut suggestions = Vec::new();
    if is_match(r"html\.match\(/<title>\(\.\*\?\)</title>/i\)", message) {
        suggestions.push(Suggestion {
            message: "In regular expressions, the angle brackets of HTML tags need to be escaped. Please modify `</title>` to `</title>`",
            example: Some((
                "html.match(/<title>(.*?)</title>/i)",
                "html.match(/<title>(.*?)<\\/title>/i)",
            )),
        });
    }
    if is_match(r"Parse error|SyntaxError|Unexpected token", message) {
        suggestions.push(Suggestion {
            message: "Check the code syntax, ensure that the parentheses, quotes, semicolons, etc. are correctly paired",
            example: None,
        });
    }
    if is_match(r"Cannot resolve module|Module not found", message) {
        suggestions.push(Suggestion {
            message: "Check the import path to ensure that the module file exists",
            example: None,
        });
    }
    if is_match(r"Type error|Type '.*' is not assignable", message) {
        suggestions.push(Suggestion {
            message: "Check the variable type definition, ensure that the type matches",
            example: None,
        });
    }
    if is_match(r"Cannot find module|Module not found", message) {
        suggestions.push(Suggestion {
            message: "Run `pnpm install` to install the missing dependency package",
            example: None,
        });
    }
    if suggestions.is_empty() {
        suggestions.push(Suggestion {
            message: "Please carefully check the files and line numbers mentioned in the error information, ensure that the code syntax is correct",
            example: None,
        });
    }
    suggestions
}

fn render(
    details: &ErrorDetails,
    file_info: Option<&FileInfo>,
    suggestions: &[Suggestion],
) -> String {
    let mut output = format!(
        "Build failed!\n\nError type: {}\nError description: {}\n\n",
        details.kind, details.message
    );
    if let Some(info) = file_info {
        output.push_str(&format!(
            "📍 Error location:\n   File: {}\n   Line: {}, Column: {}\n\n",
            basename(&info.path),
            info.line,
            info.column
        ));
    }
    output.push_str("🔧 Repair suggestions:\n");
    for (index, suggestion) in suggestions.iter().enumerate() {
        output.push_str(&format!("   {}. {}\n", index + 1, suggestion.message));
        if let Some((wrong, correct)) = suggestion.example {
            output.push_str(&format!(
                "      Wrong code: {wrong}\n      Correct code: {correct}\n"
            ));
        }
    }
    if let Some(info) = file_info {
        output.push_str(&format!(
            "   {}. Please check the code near line {} column {} in file {}\n",
            suggestions.len() + 1,
            info.line,
            info.column,
            basename(&info.path)
        ));
    }
    output.push_str(
        "\n💡 Operation steps:\n   1. Please modify the code according to the above suggestions\n   2. Save the file and rebuild the project\n   3. If the problem still exists, please check other related files\n\n📞 Need help?\n   If you cannot solve this problem, please contact technical support and provide complete error information.",
    );
    output
}

fn basename(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

fn is_match(pattern: &str, value: &str) -> bool {
    Regex::new(pattern)
        .map(|regex| regex.is_match(value))
        .unwrap_or_else(|error| {
            tracing::error!(%error, %pattern, "invalid built-in build-error regex");
            false
        })
}

fn captures<'a>(pattern: &str, value: &'a str) -> Option<regex::Captures<'a>> {
    Regex::new(pattern)
        .map_err(|error| {
            tracing::error!(%error, %pattern, "invalid built-in build-error regex");
        })
        .ok()?
        .captures(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_error_matches_nuwax_message() {
        let message = parse("Error: Cannot find module 'react-dom'");
        assert_eq!(
            message,
            "Build failed!\n\nError type: Build error\nError description: Error: Cannot find module 'react-dom'\n\n🔧 Repair suggestions:\n   1. Run `pnpm install` to install the missing dependency package\n\n💡 Operation steps:\n   1. Please modify the code according to the above suggestions\n   2. Save the file and rebuild the project\n   3. If the problem still exists, please check other related files\n\n📞 Need help?\n   If you cannot solve this problem, please contact technical support and provide complete error information."
        );
    }

    #[test]
    fn includes_file_location_and_suggestion() {
        let message = parse("file: /workspace/src/App.tsx:10:5\nSyntaxError: bad token");
        assert!(message.contains("File: App.tsx"));
        assert!(message.contains("Line: 10, Column: 5"));
        assert!(message.contains("Please check the code near line 10 column 5"));
    }
}
