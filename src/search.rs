//! 全局搜索：优先调用 ripgrep 二进制，缺失时回退到内置字节级扫描。
//!
//! 性能要点：
//! - ripgrep 用 `spawn` + 逐行读 stdout，边搜边推结果，不用等进程结束；
//! - 内置回退用 `regex::bytes` 直接扫字节，避免旧实现里每行 `to_lowercase()`
//!   带来的海量临时分配（这是大目录搜索慢的主因）；
//! - 结果逐条通过 channel 送回，UI 每帧只取有限条，不阻塞渲染。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};

use regex::bytes::RegexBuilder;
use serde_json::Value;
use walkdir::WalkDir;

/// 结果上限：再多也没人看得完，反而吃内存。
pub const MAX_HITS: usize = 3000;
/// 单个文件超过这个大小就跳过（正常源码不会这么大）。
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// 每帧最多从 channel 取多少条，避免一次涌入导致卡顿。
const DRAIN_PER_FRAME: usize = 400;

const SKIP_DIRS: [&str; 10] = [
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".idea",
    ".vscode",
    "__pycache__",
    ".venv",
    "venv",
];

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub path: PathBuf,
    pub line: usize,
    /// 列号（1 基字符列），保留给后续精确列定位
    #[allow(dead_code)]
    pub col: usize,
    pub preview: String,
}

enum Msg {
    Hit(Box<SearchHit>),
    Done(String),
    Failed(String),
}

pub struct SearchState {
    pub query: String,
    pub case_sensitive: bool,
    pub regex: bool,
    pub hits: Vec<SearchHit>,
    pub busy: bool,
    pub info: String,
    pub engine: String,
    /// 本次搜索耗时（毫秒），完成后显示
    pub elapsed_ms: u128,
    started_at: Option<std::time::Instant>,
    /// 生成序号，用来丢弃上一次搜索的残留结果
    pub generation: u64,
    rx: Option<Receiver<Msg>>,
    pub truncated: bool,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            query: String::new(),
            case_sensitive: false,
            regex: false,
            hits: Vec::new(),
            busy: false,
            info: String::new(),
            engine: find_rg()
                .is_some()
                .then(|| "ripgrep".to_string())
                .unwrap_or_else(|| "内置".into()),
            elapsed_ms: 0,
            started_at: None,
            generation: 0,
            rx: None,
            truncated: false,
        }
    }
}

impl SearchState {
    pub fn start(&mut self, root: &Path) {
        let query = self.query.trim().to_string();
        if query.is_empty() {
            self.hits.clear();
            self.info.clear();
            self.busy = false;
            self.rx = None;
            return;
        }
        self.generation += 1;
        self.busy = true;
        self.hits.clear();
        self.truncated = false;
        self.info = "搜索中…".into();
        self.elapsed_ms = 0;
        self.started_at = Some(std::time::Instant::now());

        let (tx, rx) = channel();
        self.rx = Some(rx);

        let root = root.to_path_buf();
        let case = self.case_sensitive;
        let use_regex = self.regex;
        std::thread::spawn(move || {
            if let Some(rg) = find_rg() {
                match run_rg(&rg, &root, &query, case, use_regex, &tx) {
                    Ok(()) => {
                        let _ = tx.send(Msg::Done("ripgrep".into()));
                        return;
                    }
                    Err(err) => {
                        // ripgrep 出错时退回内置实现
                        let _ = tx.send(Msg::Failed(err));
                    }
                }
            }
            match run_builtin(&root, &query, case, use_regex, &tx) {
                Ok(()) => {
                    let _ = tx.send(Msg::Done("内置".into()));
                }
                Err(err) => {
                    let _ = tx.send(Msg::Failed(err));
                }
            }
        });
    }

    /// 每帧调用：把后台结果搬到前台。
    pub fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        let mut finished = false;
        let mut engine = None;
        let mut error = None;

        for _ in 0..DRAIN_PER_FRAME {
            match rx.try_recv() {
                Ok(Msg::Hit(hit)) => {
                    if self.hits.len() < MAX_HITS {
                        self.hits.push(*hit);
                    } else {
                        self.truncated = true;
                    }
                }
                Ok(Msg::Done(name)) => {
                    engine = Some(name);
                    finished = true;
                    break;
                }
                Ok(Msg::Failed(err)) => {
                    error = Some(err);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }

        if let Some(name) = engine {
            self.engine = name;
        }
        if let Some(err) = error {
            if self.hits.is_empty() {
                self.info = format!("搜索失败：{err}");
            }
        }
        if finished {
            self.busy = false;
            self.rx = None;
            if let Some(started) = self.started_at.take() {
                self.elapsed_ms = started.elapsed().as_millis();
            }
            let total = self.hits.len();
            let ms = self.elapsed_ms;
            let engine = self.engine.clone();
            self.info = if self.truncated {
                format!("{total}+ 处结果（已截断）· {ms} ms · {engine}")
            } else {
                format!("{total} 处结果 · {ms} ms · {engine}")
            };
        } else if !self.hits.is_empty() {
            self.info = format!("{} 处…", self.hits.len());
        }
    }
}

pub fn find_rg() -> Option<PathBuf> {
    if let Some(p) = crate::config::env_var("RG") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["rg.exe", "rg"] {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        for name in ["rg.exe", "rg"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn run_rg(
    rg: &Path,
    root: &Path,
    query: &str,
    case_sensitive: bool,
    use_regex: bool,
    tx: &Sender<Msg>,
) -> Result<(), String> {
    let mut cmd = Command::new(rg);
    cmd.arg("--json")
        .arg("--no-messages")
        .arg("--max-count=100")
        .arg("--max-filesize=8M");
    if !case_sensitive {
        cmd.arg("--ignore-case");
    }
    if !use_regex {
        cmd.arg("--fixed-strings");
    }
    cmd.arg("--").arg(query).arg(root);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("无法读取 rg 输出")?;
    let reader = BufReader::with_capacity(64 * 1024, stdout);

    let mut count = 0usize;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if count >= MAX_HITS {
            let _ = child.kill();
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("match") {
            continue;
        }
        let data = &value["data"];
        let path = data["path"]["text"]
            .as_str()
            .map(PathBuf::from)
            .or_else(|| data["path"]["bytes"].as_str().map(PathBuf::from));
        let Some(path) = path else { continue };
        let line_no = data["line_number"].as_u64().unwrap_or(0) as usize;
        let text = data["lines"]["text"].as_str().unwrap_or("").trim_end();
        let col = data["submatches"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|s| s["start"].as_u64())
            .map(|s| byte_to_col(text, s as usize))
            .unwrap_or(1);
        count += 1;
        if tx
            .send(Msg::Hit(Box::new(SearchHit {
                path,
                line: line_no,
                col,
                preview: clip_preview(text),
            })))
            .is_err()
        {
            let _ = child.kill();
            break;
        }
    }

    let _ = child.wait();
    Ok(())
}

fn clip_preview(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= 200 {
        trimmed.to_string()
    } else {
        let mut out: String = trimmed.chars().take(200).collect();
        out.push('…');
        out
    }
}

fn byte_to_col(text: &str, byte: usize) -> usize {
    text.char_indices().take_while(|(i, _)| *i < byte).count() + 1
}

fn run_builtin(
    root: &Path,
    query: &str,
    case_sensitive: bool,
    use_regex: bool,
    tx: &Sender<Msg>,
) -> Result<(), String> {
    let pattern = if use_regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    let re = RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|e| format!("正则错误：{e}"))?;

    let mut count = 0usize;

    let walker = WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| {
        if e.file_type().is_dir() && e.depth() > 0 {
            let name = e.file_name().to_string_lossy();
            !SKIP_DIRS.contains(&name.as_ref())
        } else {
            true
        }
    });

    for entry in walker.filter_map(|e| e.ok()) {
        if count >= MAX_HITS {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        // 二进制探测：前 8KB 里有 NUL 就跳过
        if bytes.iter().take(8192).any(|b| *b == 0) {
            continue;
        }

        // 行索引用 memchr 一次扫完，比逐行 split 快得多
        let line_starts: Vec<usize> = std::iter::once(0)
            .chain(memchr::memchr_iter(b'\n', &bytes).map(|i| i + 1))
            .collect();

        for m in re.find_iter(&bytes) {
            if count >= MAX_HITS {
                break;
            }
            let at = m.start();
            let line_idx = match line_starts.binary_search(&at) {
                Ok(i) => i,
                Err(i) => i.saturating_sub(1),
            };
            let line_start = line_starts[line_idx];
            let line_end = line_starts
                .get(line_idx + 1)
                .map(|e| e.saturating_sub(1))
                .unwrap_or(bytes.len());
            let raw = &bytes[line_start..line_end.max(line_start)];
            let text = String::from_utf8_lossy(raw);
            let text = text.trim_end_matches('\r');
            count += 1;
            if tx
                .send(Msg::Hit(Box::new(SearchHit {
                    path: entry.path().to_path_buf(),
                    line: line_idx + 1,
                    col: at.saturating_sub(line_start) + 1,
                    preview: clip_preview(text),
                })))
                .is_err()
            {
                return Ok(());
            }
        }
    }

    Ok(())
}
