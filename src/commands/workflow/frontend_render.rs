//! Autonomous, bounded frontend rendering and AI visual-review evidence.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{
    StateDir, create_private_dir_all, now_secs, repo_slug, write_private,
};

pub const RENDER_REPORT_SCHEMA_VERSION: u32 = 1;
pub const VISUAL_REVIEW_SCHEMA_VERSION: u32 = 1;
const MAX_PACKAGE_BYTES: u64 = 128 * 1024;
const MAX_ROUTE_SCAN_ENTRIES: usize = 4_096;
const MAX_ROUTES: usize = 8;
const MAX_CAPTURE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_REVIEW_FINDINGS: usize = 32;
const MAX_REVIEW_FINDING_BYTES: usize = 512;
const SERVER_TIMEOUT: Duration = Duration::from_secs(45);
const BROWSER_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RenderStatus {
    Passed,
    Failed,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServerKind {
    Vite,
    Next,
    Generic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderCommand {
    pub program: String,
    pub args: Vec<String>,
    pub script: String,
    pub kind: ServerKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderCapture {
    pub route: String,
    pub viewport: Viewport,
    pub actual_width: u32,
    pub actual_height: u32,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub content_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderReport {
    pub schema_version: u32,
    pub id: String,
    pub repo: PathBuf,
    pub change_fingerprint: u64,
    pub profile_fingerprint: String,
    pub generated_at: u64,
    pub status: RenderStatus,
    pub server: Option<RenderCommand>,
    pub browser: Option<String>,
    pub base_url: Option<String>,
    pub routes: Vec<String>,
    pub viewports: Vec<Viewport>,
    pub captures: Vec<RenderCapture>,
    pub notes: Vec<String>,
}

impl RenderReport {
    pub fn passed(&self) -> bool {
        self.status == RenderStatus::Passed
            && !self.captures.is_empty()
            && self.captures.len() == self.routes.len() * self.viewports.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum VisualVerdict {
    Pass,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisualReview {
    pub schema_version: u32,
    pub id: String,
    pub repo: PathBuf,
    pub change_fingerprint: u64,
    pub render_report_id: String,
    pub render_profile_fingerprint: String,
    pub reviewer: String,
    pub model: Option<String>,
    pub verdict: VisualVerdict,
    pub findings: Vec<String>,
    pub created_at: u64,
}

#[derive(Debug, Args)]
pub struct RenderArgs {
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct VisualReviewArgs {
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long, value_enum)]
    pub verdict: VisualVerdict,
    /// Concrete visual finding; repeatable and bounded.
    #[arg(long = "finding")]
    pub findings: Vec<String>,
    /// Adapter or agent identity writing the structured review.
    #[arg(long, default_value = "autonomous-agent")]
    pub agent: String,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Deserialize)]
struct PackageJson {
    #[serde(default)]
    scripts: BTreeMap<String, String>,
}

fn frontend_root(state: &StateDir, repo: &Path) -> PathBuf {
    state.frontend().join(repo_slug(repo))
}

fn render_root(state: &StateDir, repo: &Path) -> PathBuf {
    frontend_root(state, repo).join("renders")
}

fn review_root(state: &StateDir, repo: &Path) -> PathBuf {
    frontend_root(state, repo).join("visual-reviews")
}

fn save_render(state: &StateDir, report: &RenderReport) -> CtxResult<()> {
    let root = render_root(state, &report.repo);
    create_private_dir_all(&root)?;
    write_private(
        &root.join(format!("{}.json", report.id)),
        &serde_json::to_string_pretty(report)?,
    )?;
    write_private(&root.join("latest"), &report.id)?;
    Ok(())
}

pub fn load_latest_render(state: &StateDir, repo: &Path) -> CtxResult<Option<RenderReport>> {
    let root = render_root(state, repo);
    let pointer = root.join("latest");
    if !pointer.exists() {
        return Ok(None);
    }
    let id = std::fs::read_to_string(pointer)?;
    let report: RenderReport = serde_json::from_str(&std::fs::read_to_string(
        root.join(format!("{}.json", id.trim())),
    )?)?;
    if report.schema_version != RENDER_REPORT_SCHEMA_VERSION {
        return Err(format!(
            "frontend render report '{}': unsupported schema_version {}",
            report.id, report.schema_version
        )
        .into());
    }
    Ok(Some(report))
}

fn save_review(state: &StateDir, review: &VisualReview) -> CtxResult<()> {
    let root = review_root(state, &review.repo);
    create_private_dir_all(&root)?;
    write_private(
        &root.join(format!("{}.json", review.id)),
        &serde_json::to_string_pretty(review)?,
    )?;
    write_private(&root.join("latest"), &review.id)?;
    Ok(())
}

pub fn load_latest_review(state: &StateDir, repo: &Path) -> CtxResult<Option<VisualReview>> {
    let root = review_root(state, repo);
    let pointer = root.join("latest");
    if !pointer.exists() {
        return Ok(None);
    }
    let id = std::fs::read_to_string(pointer)?;
    let review: VisualReview = serde_json::from_str(&std::fs::read_to_string(
        root.join(format!("{}.json", id.trim())),
    )?)?;
    if review.schema_version != VISUAL_REVIEW_SCHEMA_VERSION {
        return Err(format!(
            "frontend visual review '{}': unsupported schema_version {}",
            review.id, review.schema_version
        )
        .into());
    }
    Ok(Some(review))
}

pub fn latest_visual_is_fresh_and_passing(state: &StateDir, repo: &Path) -> CtxResult<bool> {
    let Some(render) = load_latest_render(state, repo)? else {
        return Ok(false);
    };
    let Some(review) = load_latest_review(state, repo)? else {
        return Ok(false);
    };
    let fingerprint = super::verification::change_fingerprint(repo)?;
    Ok(render.passed()
        && render.change_fingerprint == fingerprint
        && review.verdict == VisualVerdict::Pass
        && review.findings.is_empty()
        && review.change_fingerprint == fingerprint
        && review.render_report_id == render.id
        && review.render_profile_fingerprint == render.profile_fingerprint)
}

pub fn render(state: &StateDir, repo: &Path) -> CtxResult<RenderReport> {
    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let profile = super::frontend::ensure_profile(state, &repo)?;
    let change_fingerprint = super::verification::change_fingerprint(&repo)?;
    let routes = discover_routes(&repo)?;
    let viewports = vec![
        Viewport {
            width: 390,
            height: 844,
        },
        Viewport {
            width: 1440,
            height: 1000,
        },
    ];
    let mut report = RenderReport {
        schema_version: RENDER_REPORT_SCHEMA_VERSION,
        id: uuid::Uuid::new_v4().to_string(),
        repo: repo.clone(),
        change_fingerprint,
        profile_fingerprint: profile.source_fingerprint,
        generated_at: now_secs(),
        status: RenderStatus::Unavailable,
        server: None,
        browser: None,
        base_url: None,
        routes,
        viewports,
        captures: Vec::new(),
        notes: Vec::new(),
    };

    if !super::repo_gates(&repo).checks {
        report.notes.push(
            "repository-authored frontend start commands are disabled by operator policy".into(),
        );
        save_render(state, &report)?;
        return Ok(report);
    }
    let Some(server) = discover_server(&repo)? else {
        report
            .notes
            .push("no bounded frontend dev/start script was discovered".into());
        save_render(state, &report)?;
        return Ok(report);
    };
    report.server = Some(server.clone());
    if !command_available(&server.program) {
        report.notes.push(format!(
            "package runner '{}' is unavailable",
            server.program
        ));
        save_render(state, &report)?;
        return Ok(report);
    }
    let Some(browser) = discover_browser() else {
        report
            .notes
            .push("no supported local Chromium-family browser was discovered".into());
        save_render(state, &report)?;
        return Ok(report);
    };
    report.browser = Some(browser.clone());

    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let base_url = format!("http://127.0.0.1:{port}");
    report.base_url = Some(base_url.clone());
    let mut command = server_command(&server, port);
    command
        .current_dir(&repo)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("HOST", "127.0.0.1")
        .env("PORT", port.to_string())
        .env("BROWSER", "none");
    super::isolate_process_tree(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            report
                .notes
                .push(format!("frontend server could not start: {error}"));
            save_render(state, &report)?;
            return Ok(report);
        }
    };

    let result = if wait_for_server(&mut child, port)? {
        capture_all(state, &repo, &browser, &base_url, &mut report)
    } else {
        report.status = RenderStatus::Failed;
        report
            .notes
            .push("frontend server did not become ready within the bounded timeout".into());
        Ok(())
    };
    let cleanup = super::terminate_process_tree(&mut child);
    if let Err(error) = result {
        report.status = RenderStatus::Failed;
        report.notes.push(format!("capture failed: {error}"));
    }
    if let Err(error) = cleanup {
        report.status = RenderStatus::Failed;
        report
            .notes
            .push(format!("frontend server cleanup failed: {error}"));
    }
    if report.captures.len() == report.routes.len() * report.viewports.len()
        && report.status != RenderStatus::Failed
    {
        report.status = RenderStatus::Passed;
    }
    save_render(state, &report)?;
    Ok(report)
}

fn discover_server(repo: &Path) -> CtxResult<Option<RenderCommand>> {
    let path = repo.join("package.json");
    if !path.is_file() {
        return Ok(None);
    }
    let metadata = std::fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() || metadata.len() > MAX_PACKAGE_BYTES {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)?
        .take(MAX_PACKAGE_BYTES)
        .read_to_end(&mut bytes)?;
    let package: PackageJson = serde_json::from_slice(&bytes)?;
    let script = ["dev", "start", "preview"]
        .into_iter()
        .find(|name| package.scripts.contains_key(*name));
    let Some(script) = script else {
        return Ok(None);
    };
    let body = package
        .scripts
        .get(script)
        .map(String::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let kind = if body.contains("vite") {
        ServerKind::Vite
    } else if body.contains("next") {
        ServerKind::Next
    } else {
        ServerKind::Generic
    };
    let (program, args) = if repo.join("pnpm-lock.yaml").exists() {
        ("pnpm", vec!["run", script])
    } else if repo.join("yarn.lock").exists() {
        ("yarn", vec![script])
    } else if repo.join("bun.lock").exists() || repo.join("bun.lockb").exists() {
        ("bun", vec!["run", script])
    } else {
        ("npm", vec!["run", script])
    };
    Ok(Some(RenderCommand {
        program: program.into(),
        args: args.into_iter().map(str::to_string).collect(),
        script: script.into(),
        kind,
    }))
}

fn server_command(spec: &RenderCommand, port: u16) -> Command {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    let port = port.to_string();
    match spec.kind {
        ServerKind::Vite => {
            command.args(["--", "--host", "127.0.0.1", "--port", port.as_str()]);
        }
        ServerKind::Next => {
            command.args(["--", "-H", "127.0.0.1", "-p", port.as_str()]);
        }
        ServerKind::Generic => {}
    }
    command
}

fn command_available(program: &str) -> bool {
    let mut command = Command::new(program);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    super::isolate_process_tree(&mut command);
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => return false,
        }
    }
    let _ = super::terminate_process_tree(&mut child);
    false
}

fn discover_browser() -> Option<String> {
    [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "microsoft-edge",
        "msedge",
    ]
    .into_iter()
    .find(|program| command_available(program))
    .map(str::to_string)
}

fn wait_for_server(child: &mut Child, port: u16) -> CtxResult<bool> {
    let started = Instant::now();
    while started.elapsed() < SERVER_TIMEOUT {
        if child.try_wait()?.is_some() {
            return Ok(false);
        }
        if TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse()?,
            Duration::from_millis(250),
        )
        .is_ok()
        {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(false)
}

fn capture_all(
    state: &StateDir,
    repo: &Path,
    browser: &str,
    base_url: &str,
    report: &mut RenderReport,
) -> CtxResult<()> {
    let output = render_root(state, repo).join(&report.id);
    create_private_dir_all(&output)?;
    for route in &report.routes {
        for viewport in &report.viewports {
            let filename = format!(
                "{}-{}x{}.png",
                route_slug(route),
                viewport.width,
                viewport.height
            );
            let path = output.join(filename);
            let url = format!("{base_url}{route}");
            let status = capture(browser, &url, &path, *viewport)?;
            if !status {
                return Err(format!(
                    "browser failed for route '{route}' at {}x{}",
                    viewport.width, viewport.height
                )
                .into());
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_CAPTURE_BYTES {
                return Err(format!("invalid screenshot output '{}'", path.display()).into());
            }
            let (actual_width, actual_height) = png_dimensions(&path)?;
            let content_fingerprint = file_fingerprint(&path)?;
            report.captures.push(RenderCapture {
                route: route.clone(),
                viewport: *viewport,
                actual_width,
                actual_height,
                path,
                size_bytes: metadata.len(),
                content_fingerprint,
            });
        }
    }
    Ok(())
}

fn capture(browser: &str, url: &str, path: &Path, viewport: Viewport) -> CtxResult<bool> {
    let mut command = Command::new(browser);
    command
        .arg("--headless=new")
        .arg("--disable-background-networking")
        .arg("--disable-component-update")
        .arg("--disable-default-apps")
        .arg("--disable-extensions")
        .arg("--disable-sync")
        .arg("--metrics-recording-only")
        .arg("--no-first-run")
        .arg("--virtual-time-budget=2000")
        .arg("--host-resolver-rules=MAP * 0.0.0.0, EXCLUDE localhost, EXCLUDE 127.0.0.1")
        .arg(format!("--window-size={},{}", viewport.width, viewport.height))
        .arg(format!("--screenshot={}", path.display()))
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    super::isolate_process_tree(&mut command);
    let mut child = command.spawn()?;
    let started = Instant::now();
    while started.elapsed() < BROWSER_TIMEOUT {
        if let Some(status) = child.try_wait()? {
            return Ok(status.success());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    super::terminate_process_tree(&mut child)?;
    Ok(false)
}

fn png_dimensions(path: &Path) -> CtxResult<(u32, u32)> {
    let mut header = [0u8; 24];
    std::fs::File::open(path)?.read_exact(&mut header)?;
    if &header[..8] != b"\x89PNG\r\n\x1a\n" || &header[12..16] != b"IHDR" {
        return Err(format!("'{}' is not a PNG screenshot", path.display()).into());
    }
    Ok((
        u32::from_be_bytes(header[16..20].try_into()?),
        u32::from_be_bytes(header[20..24].try_into()?),
    ))
}

fn file_fingerprint(path: &Path) -> CtxResult<String> {
    let mut hash = 0xcbf29ce484222325u64;
    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        for byte in &buffer[..read] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    Ok(format!("{hash:016x}"))
}

fn route_slug(route: &str) -> String {
    let value = route
        .trim_matches('/')
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    if value.is_empty() {
        "root".into()
    } else {
        value
    }
}

fn discover_routes(repo: &Path) -> CtxResult<Vec<String>> {
    let mut files = Vec::new();
    let mut entries = 0usize;
    collect_route_files(repo, repo, &mut entries, &mut files)?;
    let mut routes = BTreeSet::from(["/".to_string()]);
    for path in files {
        let relative = path.strip_prefix(repo).unwrap_or(&path);
        let components = relative
            .components()
            .filter_map(|component| component.as_os_str().to_str())
            .collect::<Vec<_>>();
        if let Some(index) = components
            .iter()
            .position(|component| matches!(*component, "app" | "pages"))
        {
            let mut parts = components[index + 1..].to_vec();
            let Some(file) = parts.pop() else { continue };
            let stem = Path::new(file)
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            let is_app_page = components[index] == "app" && stem == "page";
            let is_pages_route = components[index] == "pages"
                && !stem.starts_with('_')
                && !parts.first().is_some_and(|part| *part == "api");
            if !is_app_page && !is_pages_route {
                continue;
            }
            if is_pages_route && stem != "index" {
                parts.push(stem);
            }
            parts.retain(|part| !part.starts_with('('));
            if parts.iter().any(|part| part.contains('[')) {
                continue;
            }
            routes.insert(format!("/{}", parts.join("/")));
            if routes.len() >= MAX_ROUTES {
                break;
            }
        }
    }
    Ok(routes.into_iter().take(MAX_ROUTES).collect())
}

fn collect_route_files(
    repo: &Path,
    directory: &Path,
    entries: &mut usize,
    files: &mut Vec<PathBuf>,
) -> CtxResult<()> {
    if *entries >= MAX_ROUTE_SCAN_ENTRIES || files.len() >= MAX_ROUTE_SCAN_ENTRIES {
        return Ok(());
    }
    let mut children = std::fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        if *entries >= MAX_ROUTE_SCAN_ENTRIES {
            break;
        }
        *entries += 1;
        let path = child.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            let name = child.file_name();
            if !matches!(
                name.to_str(),
                Some(
                    ".git"
                        | ".zirv"
                        | "node_modules"
                        | "target"
                        | "dist"
                        | "build"
                        | ".next"
                        | ".cache"
                        | "vendor"
                )
            ) {
                collect_route_files(repo, &path, entries, files)?;
            }
        } else if metadata.is_file()
            && matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("js" | "jsx" | "ts" | "tsx" | "vue" | "svelte")
            )
        {
            files.push(path);
        }
    }
    Ok(())
}

fn record_review(
    state: &StateDir,
    repo: &Path,
    args: &VisualReviewArgs,
) -> CtxResult<VisualReview> {
    validate_review_input(args.verdict, &args.findings)?;
    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let render = load_latest_render(state, &repo)?.ok_or("no frontend render evidence")?;
    let fingerprint = super::verification::change_fingerprint(&repo)?;
    if !render.passed() || render.change_fingerprint != fingerprint {
        return Err("visual review requires a fresh passing frontend render".into());
    }
    let review = VisualReview {
        schema_version: VISUAL_REVIEW_SCHEMA_VERSION,
        id: uuid::Uuid::new_v4().to_string(),
        repo,
        change_fingerprint: fingerprint,
        render_report_id: render.id,
        render_profile_fingerprint: render.profile_fingerprint,
        reviewer: args.agent.clone(),
        model: args.model.clone(),
        verdict: args.verdict,
        findings: args.findings.clone(),
        created_at: now_secs(),
    };
    save_review(state, &review)?;
    Ok(review)
}

fn validate_review_input(verdict: VisualVerdict, findings: &[String]) -> CtxResult<()> {
    if findings.len() > MAX_REVIEW_FINDINGS {
        return Err(format!(
            "visual review has {} findings; limit is {MAX_REVIEW_FINDINGS}",
            findings.len()
        )
        .into());
    }
    if let Some(finding) = findings
        .iter()
        .find(|finding| finding.len() > MAX_REVIEW_FINDING_BYTES)
    {
        return Err(format!(
            "visual review finding is {} bytes; limit is {MAX_REVIEW_FINDING_BYTES}",
            finding.len()
        )
        .into());
    }
    if verdict == VisualVerdict::Pass && !findings.is_empty() {
        return Err("a passing visual review cannot contain unresolved findings".into());
    }
    if verdict == VisualVerdict::Fail && findings.is_empty() {
        return Err("a failing visual review requires at least one concrete finding".into());
    }
    Ok(())
}

fn write_render(writer: &mut impl Write, report: &RenderReport, json: bool) -> CtxResult<()> {
    if json {
        serde_json::to_writer_pretty(&mut *writer, report)?;
        writeln!(writer)?;
    } else {
        writeln!(
            writer,
            "frontend render: {:?} ({} captures)",
            report.status,
            report.captures.len()
        )?;
        for capture in &report.captures {
            writeln!(
                writer,
                "{}\t{}x{}\t{}",
                capture.route,
                capture.viewport.width,
                capture.viewport.height,
                capture.path.display()
            )?;
        }
        for note in &report.notes {
            writeln!(writer, "note: {note}")?;
        }
    }
    Ok(())
}

fn write_review(writer: &mut impl Write, review: &VisualReview, json: bool) -> CtxResult<()> {
    if json {
        serde_json::to_writer_pretty(&mut *writer, review)?;
        writeln!(writer)?;
    } else {
        writeln!(writer, "frontend visual review: {:?}", review.verdict)?;
        writeln!(writer, "render: {}", review.render_report_id)?;
        for finding in &review.findings {
            writeln!(writer, "- {finding}")?;
        }
    }
    Ok(())
}

pub fn run_render(args: &RenderArgs, writer: &mut impl Write) -> CtxResult<i32> {
    let repo = args
        .repo
        .clone()
        .unwrap_or(std::env::current_dir()?);
    let state = StateDir::resolve(&|key| std::env::var(key).ok())?;
    let report = render(&state, &repo)?;
    write_render(writer, &report, args.json)?;
    Ok(if report.passed() { 0 } else { 1 })
}

pub fn run_review(args: &VisualReviewArgs, writer: &mut impl Write) -> CtxResult<i32> {
    let repo = args
        .repo
        .clone()
        .unwrap_or(std::env::current_dir()?);
    let state = StateDir::resolve(&|key| std::env::var(key).ok())?;
    let review = record_review(&state, &repo, args)?;
    write_review(writer, &review, args.json)?;
    Ok(if review.verdict == VisualVerdict::Pass {
        0
    } else {
        1
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_discovery_is_bounded_and_uses_argv_not_a_shell() {
        let repo = tempfile::tempdir().expect("repo");
        std::fs::write(
            repo.path().join("package.json"),
            r#"{"scripts":{"dev":"vite --clearScreen false"}}"#,
        )
        .expect("package");

        let server = discover_server(repo.path()).expect("discovery").expect("server");

        assert_eq!(server.program, "npm");
        assert_eq!(server.args, ["run", "dev"]);
        assert_eq!(server.kind, ServerKind::Vite);
    }

    #[test]
    fn route_discovery_skips_dynamic_routes_that_need_human_fixtures() {
        let repo = tempfile::tempdir().expect("repo");
        for file in [
            "src/app/page.tsx",
            "src/app/settings/page.tsx",
            "src/app/users/[id]/page.tsx",
        ] {
            let path = repo.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).expect("directory");
            std::fs::write(path, "export default function Page() {}").expect("page");
        }

        let routes = discover_routes(repo.path()).expect("routes");

        assert!(routes.contains(&"/".to_string()));
        assert!(routes.contains(&"/settings".to_string()));
        assert!(!routes.iter().any(|route| route.contains("[id]")));
    }

    #[test]
    fn visual_review_findings_are_bounded_and_pass_cannot_hide_them() {
        assert!(
            validate_review_input(VisualVerdict::Pass, &["alignment defect".into()]).is_err()
        );
        assert!(validate_review_input(VisualVerdict::Fail, &[]).is_err());
        assert!(validate_review_input(VisualVerdict::Pass, &[]).is_ok());
    }
}
