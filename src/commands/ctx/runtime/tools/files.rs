use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{CapturePayload, ToolError, ToolErrorCode};
use crate::commands::ctx::output::CompactionScope;
use crate::commands::ctx::state;

const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_WALK_ENTRIES: usize = 100_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadFileArgs {
    pub path: PathBuf,
    #[serde(default = "one")]
    pub start_line: usize,
    #[serde(default)]
    pub end_line: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DirectoryArgs {
    pub path: PathBuf,
    #[serde(default)]
    pub recursive: bool,
    #[serde(default = "default_results")]
    pub max_results: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GlobArgs {
    pub root: PathBuf,
    pub pattern: String,
    #[serde(default = "default_results")]
    pub max_results: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SearchArgs {
    pub root: PathBuf,
    pub query: String,
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub include: Option<String>,
    #[serde(default = "default_results")]
    pub max_results: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WriteFileArgs {
    pub path: PathBuf,
    pub content: String,
    #[serde(default)]
    pub expected_sha256: Option<String>,
    #[serde(default)]
    pub create_only: bool,
    pub idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ApplyPatchArgs {
    pub path: PathBuf,
    pub expected_sha256: String,
    pub operations: Vec<ReplaceOperation>,
    pub idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReplaceOperation {
    pub expected: String,
    pub replacement: String,
    #[serde(default = "one")]
    pub expected_occurrences: usize,
}

#[derive(Debug)]
pub(super) struct FileOutcome {
    pub data: Value,
    pub capture: Option<CapturePayload>,
}

pub(super) fn read_file(
    path: &Path,
    args: &ReadFileArgs,
    max_inline_bytes: usize,
) -> Result<FileOutcome, ToolError> {
    if args.start_line == 0 || args.end_line.is_some_and(|end| end < args.start_line) {
        return Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            "line ranges are 1-based and end_line must not precede start_line",
        ));
    }
    let bytes = read_bounded(path)?;
    let sha256 = sha256(&bytes);
    let media_type = media_type(path, &bytes);
    let Some((text, encoding)) = decode_text(&bytes) else {
        return Ok(FileOutcome {
            data: json!({
                "path": path,
                "kind": if media_type.starts_with("image/") { "image" } else { "binary" },
                "media_type": media_type,
                "bytes": bytes.len(),
                "sha256": sha256,
                "content": Value::Null,
            }),
            capture: Some(CapturePayload::new(
                bytes,
                vec!["native:file_read".into(), path.display().to_string()],
                CompactionScope::Verbatim,
            )),
        });
    };

    let ending = line_ending(&text);
    let end = args.end_line.unwrap_or(usize::MAX);
    let mut selected = String::new();
    let mut total_lines = 0usize;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let number = index + 1;
        total_lines = number;
        if number >= args.start_line && number <= end {
            selected.push_str(line);
        }
    }
    if !text.is_empty() && !text.ends_with('\n') {
        total_lines = total_lines.max(1);
    }
    let truncated = selected.len() > max_inline_bytes;
    let inline = if truncated {
        prefix_at_boundary(&selected, max_inline_bytes)
    } else {
        selected.as_str()
    };
    Ok(FileOutcome {
        data: json!({
            "path": path,
            "kind": "text",
            "media_type": media_type,
            "encoding": encoding,
            "line_ending": ending,
            "bytes": bytes.len(),
            "sha256": sha256,
            "total_lines": total_lines,
            "start_line": args.start_line,
            "end_line": end.min(total_lines),
            "content": inline,
            "truncated": truncated,
        }),
        capture: truncated.then(|| {
            CapturePayload::new(
                bytes,
                vec!["native:file_read".into(), path.display().to_string()],
                CompactionScope::Verbatim,
            )
        }),
    })
}

pub(super) fn list_directory(root: &Path, args: &DirectoryArgs) -> Result<FileOutcome, ToolError> {
    let limit = args.max_results.clamp(1, MAX_WALK_ENTRIES);
    let mut entries = Vec::new();
    walk(root, args.recursive, &mut |path, metadata| {
        entries.push(json!({
            "path": relative(root, path),
            "kind": file_kind(metadata),
            "bytes": metadata.is_file().then_some(metadata.len()),
        }));
        entries.len() < MAX_WALK_ENTRIES
    })?;
    entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    let total = entries.len();
    let returned: Vec<Value> = entries.iter().take(limit).cloned().collect();
    let capture = (total > limit).then(|| {
        let full = entries
            .iter()
            .filter_map(|entry| entry["path"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        CapturePayload::new(
            full.into_bytes(),
            vec!["native:directory_list".into(), root.display().to_string()],
            CompactionScope::Shape,
        )
    });
    Ok(FileOutcome {
        data: json!({
            "root": root,
            "entries": returned,
            "total": total,
            "truncated": total > limit,
        }),
        capture,
    })
}

pub(super) fn glob(root: &Path, args: &GlobArgs) -> Result<FileOutcome, ToolError> {
    if args.pattern.trim().is_empty() || Path::new(&args.pattern).is_absolute() {
        return Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            "glob pattern must be a non-empty relative pattern",
        ));
    }
    let limit = args.max_results.clamp(1, MAX_WALK_ENTRIES);
    let mut matches = Vec::new();
    walk(root, true, &mut |path, _| {
        let relative = relative(root, path);
        if glob_matches(
            &args.pattern.replace('\\', "/"),
            &relative.replace('\\', "/"),
        ) {
            matches.push(relative);
        }
        matches.len() < MAX_WALK_ENTRIES
    })?;
    matches.sort();
    matches.dedup();
    string_list_outcome("glob_search", root, matches, limit)
}

pub(super) fn search(root: &Path, args: &SearchArgs) -> Result<FileOutcome, ToolError> {
    if args.query.is_empty() {
        return Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            "search query must not be empty",
        ));
    }
    let include = args
        .include
        .as_ref()
        .map(|pattern| pattern.replace('\\', "/"));
    let regex =
        if args.regex {
            let pattern = if args.case_sensitive {
                args.query.clone()
            } else {
                format!("(?i:{})", args.query)
            };
            Some(regex::Regex::new(&pattern).map_err(|error| {
                ToolError::new(ToolErrorCode::InvalidArguments, error.to_string())
            })?)
        } else {
            None
        };
    let needle = (!args.case_sensitive && !args.regex).then(|| args.query.to_lowercase());
    let limit = args.max_results.clamp(1, MAX_WALK_ENTRIES);
    let mut matches = Vec::new();
    let mut skipped_binary = 0usize;
    walk(root, true, &mut |path, metadata| {
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            return true;
        }
        let relative_path = relative(root, path).replace('\\', "/");
        if include
            .as_ref()
            .is_some_and(|pattern| !glob_matches(pattern, &relative_path))
        {
            return true;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return true;
        };
        let Some((text, _)) = decode_text(&bytes) else {
            skipped_binary += 1;
            return true;
        };
        for (index, line) in text.lines().enumerate() {
            let found = if let Some(regex) = &regex {
                regex.find(line).map(|hit| hit.start())
            } else if args.case_sensitive {
                line.find(&args.query)
            } else {
                line.to_lowercase()
                    .find(needle.as_deref().unwrap_or_default())
            };
            if let Some(column) = found {
                matches.push(json!({
                    "path": relative_path,
                    "line": index + 1,
                    "column": column + 1,
                    "text": line,
                }));
                if matches.len() >= MAX_WALK_ENTRIES {
                    return false;
                }
            }
        }
        true
    })?;
    let total = matches.len();
    let returned: Vec<Value> = matches.iter().take(limit).cloned().collect();
    let capture = (total > limit).then(|| {
        let mut full = String::new();
        for item in &matches {
            full.push_str(&format!(
                "{}:{}:{}:{}\n",
                item["path"].as_str().unwrap_or_default(),
                item["line"].as_u64().unwrap_or_default(),
                item["column"].as_u64().unwrap_or_default(),
                item["text"].as_str().unwrap_or_default()
            ));
        }
        CapturePayload::new(
            full.into_bytes(),
            vec!["native:text_search".into(), args.query.clone()],
            CompactionScope::Shape,
        )
    });
    Ok(FileOutcome {
        data: json!({
            "root": root,
            "matches": returned,
            "total": total,
            "truncated": total > limit,
            "skipped_binary_files": skipped_binary,
        }),
        capture,
    })
}

pub(super) fn write_file(path: &Path, args: &WriteFileArgs) -> Result<FileOutcome, ToolError> {
    validate_key(&args.idempotency_key)?;
    let current = match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(ToolError::io(error)),
    };
    if args.create_only && current.is_some() {
        return Err(ToolError::new(
            ToolErrorCode::PreconditionFailed,
            "create_only target already exists",
        ));
    }
    let (encoding, ending) = current
        .as_deref()
        .and_then(decode_text)
        .map(|(text, encoding)| (encoding, line_ending(&text)))
        .unwrap_or((TextEncoding::Utf8, "none"));
    let content = normalize_line_endings(&args.content, ending);
    let desired = encode_text(&content, encoding);
    let desired_sha = sha256(&desired);
    if current
        .as_deref()
        .is_some_and(|bytes| sha256(bytes) == desired_sha)
    {
        return Ok(FileOutcome {
            data: json!({
                "path": path,
                "sha256": desired_sha,
                "bytes": desired.len(),
                "already_applied": true,
                "idempotency_key": args.idempotency_key,
            }),
            capture: None,
        });
    }
    match (&current, &args.expected_sha256) {
        (Some(bytes), Some(expected)) if &sha256(bytes) != expected => {
            return Err(stale(expected, &sha256(bytes)));
        }
        (Some(_), None) if !args.create_only => {
            return Err(ToolError::new(
                ToolErrorCode::PreconditionFailed,
                "expected_sha256 is required when replacing an existing file",
            ));
        }
        (None, Some(expected)) if !expected.is_empty() => {
            return Err(stale(expected, "missing"));
        }
        _ => {}
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(ToolError::io)?;
    }
    state::write_atomic_bytes(path, &desired, false).map_err(ToolError::io)?;
    Ok(FileOutcome {
        data: json!({
            "path": path,
            "sha256": desired_sha,
            "bytes": desired.len(),
            "already_applied": false,
            "idempotency_key": args.idempotency_key,
        }),
        capture: None,
    })
}

pub(super) fn apply_patch(path: &Path, args: &ApplyPatchArgs) -> Result<FileOutcome, ToolError> {
    validate_key(&args.idempotency_key)?;
    if args.expected_sha256.is_empty() || args.operations.is_empty() {
        return Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            "expected_sha256 and at least one replacement operation are required",
        ));
    }
    let bytes = std::fs::read(path).map_err(ToolError::io)?;
    let actual_sha = sha256(&bytes);
    let (mut text, encoding) = decode_text(&bytes).ok_or_else(|| {
        ToolError::new(
            ToolErrorCode::UnsupportedContent,
            "structured patches require UTF-8 or BOM-marked UTF-16 text",
        )
    })?;
    if actual_sha != args.expected_sha256 {
        return Err(stale(&args.expected_sha256, &actual_sha));
    }
    let ending = line_ending(&text);
    for operation in &args.operations {
        if operation.expected.is_empty() || operation.expected_occurrences == 0 {
            return Err(ToolError::new(
                ToolErrorCode::InvalidArguments,
                "replacement expected text must be non-empty and expected_occurrences positive",
            ));
        }
        let expected = normalize_line_endings(&operation.expected, ending);
        let replacement = normalize_line_endings(&operation.replacement, ending);
        let found = text.match_indices(&expected).count();
        if found != operation.expected_occurrences {
            return Err(ToolError::new(
                ToolErrorCode::PreconditionFailed,
                format!(
                    "replacement expected {} occurrence(s) but found {found}",
                    operation.expected_occurrences
                ),
            ));
        }
        text = text.replacen(&expected, &replacement, operation.expected_occurrences);
    }
    let desired = encode_text(&text, encoding);
    let desired_sha = sha256(&desired);
    state::write_atomic_bytes(path, &desired, false).map_err(ToolError::io)?;
    Ok(FileOutcome {
        data: json!({
            "path": path,
            "before_sha256": actual_sha,
            "sha256": desired_sha,
            "bytes": desired.len(),
            "operations": args.operations.len(),
            "idempotency_key": args.idempotency_key,
        }),
        capture: None,
    })
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, ToolError> {
    let metadata = std::fs::metadata(path).map_err(ToolError::io)?;
    if !metadata.is_file() {
        return Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            "file_read target is not a regular file",
        ));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(ToolError::new(
            ToolErrorCode::OutputLimit,
            format!(
                "file is {} bytes; hard limit is {MAX_FILE_BYTES}",
                metadata.len()
            ),
        ));
    }
    std::fs::read(path).map_err(ToolError::io)
}

fn walk(
    root: &Path,
    recursive: bool,
    visit: &mut impl FnMut(&Path, &std::fs::Metadata) -> bool,
) -> Result<(), ToolError> {
    if root.is_file() {
        let metadata = std::fs::symlink_metadata(root).map_err(ToolError::io)?;
        visit(root, &metadata);
        return Ok(());
    }
    let mut pending = vec![root.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir).map_err(ToolError::io)?;
        for entry in entries {
            let entry = entry.map_err(ToolError::io)?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(ToolError::io)?;
            seen += 1;
            if !visit(&path, &metadata) || seen >= MAX_WALK_ENTRIES {
                return Ok(());
            }
            if recursive && metadata.is_dir() && !metadata.file_type().is_symlink() {
                pending.push(path);
            }
        }
        if !recursive {
            break;
        }
    }
    Ok(())
}

fn string_list_outcome(
    tool: &str,
    root: &Path,
    values: Vec<String>,
    limit: usize,
) -> Result<FileOutcome, ToolError> {
    let total = values.len();
    let returned: Vec<&String> = values.iter().take(limit).collect();
    let capture = (total > limit).then(|| {
        CapturePayload::new(
            (values.join("\n") + "\n").into_bytes(),
            vec![format!("native:{tool}"), root.display().to_string()],
            CompactionScope::Shape,
        )
    });
    Ok(FileOutcome {
        data: json!({
            "root": root,
            "matches": returned,
            "total": total,
            "truncated": total > limit,
        }),
        capture,
    })
}

fn file_kind(metadata: &std::fs::Metadata) -> &'static str {
    if metadata.file_type().is_symlink() {
        "symlink"
    } else if metadata.is_dir() {
        "directory"
    } else if metadata.is_file() {
        "file"
    } else {
        "other"
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    fn matches(pattern: &[char], path: &[char]) -> bool {
        if pattern.is_empty() {
            return path.is_empty();
        }
        if pattern.starts_with(&['*', '*']) {
            let rest = if pattern.get(2) == Some(&'/') {
                &pattern[3..]
            } else {
                &pattern[2..]
            };
            return matches(rest, path) || (!path.is_empty() && matches(pattern, &path[1..]));
        }
        match pattern[0] {
            '*' => {
                matches(&pattern[1..], path)
                    || (!path.is_empty() && path[0] != '/' && matches(pattern, &path[1..]))
            }
            '?' => !path.is_empty() && path[0] != '/' && matches(&pattern[1..], &path[1..]),
            literal => !path.is_empty() && literal == path[0] && matches(&pattern[1..], &path[1..]),
        }
    }
    matches(
        &pattern.chars().collect::<Vec<_>>(),
        &path.chars().collect::<Vec<_>>(),
    )
}

#[derive(Clone, Copy, Debug)]
enum TextEncoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

impl TextEncoding {
    fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf8Bom => "utf-8-bom",
            Self::Utf16Le => "utf-16le",
            Self::Utf16Be => "utf-16be",
        }
    }
}

impl serde::Serialize for TextEncoding {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.label())
    }
}

fn decode_text(bytes: &[u8]) -> Option<(String, TextEncoding)> {
    if let Some(body) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        return String::from_utf8(body.to_vec())
            .ok()
            .map(|text| (text, TextEncoding::Utf8Bom));
    }
    if let Some(body) = bytes.strip_prefix(&[0xff, 0xfe]) {
        if body.len() % 2 != 0 {
            return None;
        }
        let words = body
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
        return char::decode_utf16(words)
            .collect::<Result<String, _>>()
            .ok()
            .map(|text| (text, TextEncoding::Utf16Le));
    }
    if let Some(body) = bytes.strip_prefix(&[0xfe, 0xff]) {
        if body.len() % 2 != 0 {
            return None;
        }
        let words = body
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]));
        return char::decode_utf16(words)
            .collect::<Result<String, _>>()
            .ok()
            .map(|text| (text, TextEncoding::Utf16Be));
    }
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes.to_vec())
        .ok()
        .map(|text| (text, TextEncoding::Utf8))
}

fn encode_text(text: &str, encoding: TextEncoding) -> Vec<u8> {
    match encoding {
        TextEncoding::Utf8 => text.as_bytes().to_vec(),
        TextEncoding::Utf8Bom => [vec![0xef, 0xbb, 0xbf], text.as_bytes().to_vec()].concat(),
        TextEncoding::Utf16Le => {
            let mut bytes = vec![0xff, 0xfe];
            bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
            bytes
        }
        TextEncoding::Utf16Be => {
            let mut bytes = vec![0xfe, 0xff];
            bytes.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
            bytes
        }
    }
}

fn line_ending(text: &str) -> &'static str {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count().saturating_sub(crlf);
    let cr = text.matches('\r').count().saturating_sub(crlf);
    match (crlf, lf, cr) {
        (0, 0, 0) => "none",
        (n, 0, 0) if n > 0 => "crlf",
        (0, n, 0) if n > 0 => "lf",
        (0, 0, n) if n > 0 => "cr",
        _ => "mixed",
    }
}

fn normalize_line_endings(text: &str, ending: &str) -> String {
    if !matches!(ending, "crlf" | "lf" | "cr") {
        return text.to_string();
    }
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    match ending {
        "crlf" => normalized.replace('\n', "\r\n"),
        "cr" => normalized.replace('\n', "\r"),
        _ => normalized,
    }
}

fn media_type(path: &Path, bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else if path.extension().and_then(|ext| ext.to_str()) == Some("svg") {
        "image/svg+xml"
    } else if decode_text(bytes).is_some() {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}

fn validate_key(key: &str) -> Result<(), ToolError> {
    if key.trim().is_empty() || key.len() > 256 || key.contains('\0') {
        Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            "idempotency_key must contain 1..=256 non-NUL bytes",
        ))
    } else {
        Ok(())
    }
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn stale(expected: &str, actual: &str) -> ToolError {
    ToolError::new(
        ToolErrorCode::PreconditionFailed,
        format!("stale file: expected sha256 {expected}, found {actual}"),
    )
}

fn prefix_at_boundary(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn one() -> usize {
    1
}

fn default_results() -> usize {
    200
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn glob_double_star_crosses_directories_but_single_star_does_not() {
        assert!(glob_matches("src/**/*.rs", "src/runtime/tools.rs"));
        assert!(glob_matches("src/*.rs", "src/lib.rs"));
        assert!(!glob_matches("src/*.rs", "src/runtime/lib.rs"));
    }

    #[test]
    fn patch_preserves_utf16_and_crlf_and_refuses_stale_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("unicode.txt");
        let original = encode_text("æble\r\nold\r\n", TextEncoding::Utf16Le);
        std::fs::write(&path, &original).expect("write");
        let args = ApplyPatchArgs {
            path: path.clone(),
            expected_sha256: sha256(&original),
            operations: vec![ReplaceOperation {
                expected: "old\n".into(),
                replacement: "ny\n".into(),
                expected_occurrences: 1,
            }],
            idempotency_key: "edit-1".into(),
        };
        apply_patch(&path, &args).expect("patch");
        let changed = std::fs::read(&path).expect("read");
        assert!(changed.starts_with(&[0xff, 0xfe]));
        assert_eq!(decode_text(&changed).expect("decode").0, "æble\r\nny\r\n");
        let error = apply_patch(&path, &args).expect_err("stale patch must fail");
        assert_eq!(error.code, ToolErrorCode::PreconditionFailed);
    }

    #[test]
    fn binary_read_is_explicit_and_keeps_bytes_for_the_output_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pixel.png");
        let bytes = b"\x89PNG\r\n\x1a\n\0binary";
        std::fs::write(&path, bytes).expect("write");
        let outcome = read_file(
            &path,
            &ReadFileArgs {
                path: path.clone(),
                start_line: 1,
                end_line: None,
            },
            1024,
        )
        .expect("read");
        assert_eq!(outcome.data["kind"], "image");
        assert_eq!(outcome.data["media_type"], "image/png");
        assert_eq!(outcome.capture.expect("capture").bytes, bytes);
    }

    #[test]
    fn write_requires_a_precondition_but_reconciles_an_applied_retry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "before\n").expect("write");
        let missing = WriteFileArgs {
            path: path.clone(),
            content: "after\n".into(),
            expected_sha256: None,
            create_only: false,
            idempotency_key: "write-1".into(),
        };
        assert_eq!(
            write_file(&path, &missing).expect_err("precondition").code,
            ToolErrorCode::PreconditionFailed
        );
        let before = std::fs::read(&path).expect("read");
        let args = WriteFileArgs {
            expected_sha256: Some(sha256(&before)),
            ..missing
        };
        write_file(&path, &args).expect("write");
        assert_eq!(
            write_file(&path, &args).expect("reconcile").data["already_applied"],
            true
        );
    }

    #[test]
    fn recursive_walk_never_follows_symlink_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("secret"), "no").expect("secret");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("link")).expect("symlink");
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(root.join("link"))
                .arg(&outside)
                .status()
                .expect("junction command");
            assert!(status.success(), "Windows CI must support a test junction");
        }
        let outcome = list_directory(
            &root,
            &DirectoryArgs {
                path: root.clone(),
                recursive: true,
                max_results: 20,
            },
        )
        .expect("list");
        let rendered = outcome.data.to_string();
        assert!(rendered.contains("link"));
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn search_handles_unicode_paths_and_skips_non_utf8_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("blå.rs"), "fn mål() {}\n").expect("text");
        std::fs::write(dir.path().join("binary"), [0xff, 0x00]).expect("binary");
        let outcome = search(
            dir.path(),
            &SearchArgs {
                root: dir.path().to_path_buf(),
                query: "mål".into(),
                regex: false,
                case_sensitive: true,
                include: Some("**/*.rs".into()),
                max_results: 10,
            },
        )
        .expect("search");
        assert_eq!(outcome.data["matches"][0]["path"], "blå.rs");
    }

    #[test]
    fn media_type_set_is_stable() {
        let known: BTreeSet<&str> = [
            media_type(Path::new("x.png"), b"\x89PNG\r\n\x1a\n"),
            media_type(Path::new("x"), b"hello"),
            media_type(Path::new("x"), b"\0"),
        ]
        .into_iter()
        .collect();
        assert_eq!(known.len(), 3);
    }
}
