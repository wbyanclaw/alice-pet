//! Codex 桌宠：只读 tail `~/.codex/sessions/**/*.jsonl`，把会话事件映射成宠物状态，
//! 通过 Tauri 事件推给前端（前端只负责按状态播雪碧帧）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

mod mcp;
mod tts;

const IDLE_AFTER: Duration = Duration::from_secs(45);
const SLEEP_AFTER: Duration = Duration::from_secs(180);
const FLASH: Duration = Duration::from_millis(2600);
const POLL: Duration = Duration::from_millis(900);
const DEFAULT_PORT: u16 = 8765;
const HOTKEY: &str = "ctrl+alt+p";

type Shared = Arc<Mutex<Machine>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Activity {
    Idle,
    Sleeping,
    Thinking,
    Writing,
    Tool,
    Done,
    Error,
}

#[derive(Clone, Serialize)]
struct PetState {
    activity: Activity,
    detail: String,
}

struct Machine {
    activity: Activity,
    pending_tools: i32,
    last_event: Instant,
    since_change: Instant,
    detail: String,
}

impl Machine {
    fn new() -> Self {
        let now = Instant::now();
        Self { activity: Activity::Idle, pending_tools: 0, last_event: now, since_change: now, detail: "start".into() }
    }

    fn set(&mut self, activity: Activity, detail: impl Into<String>) -> Option<PetState> {
        let detail = detail.into();
        self.last_event = Instant::now();
        if self.activity != activity || self.detail != detail {
            self.activity = activity;
            self.detail = detail.clone();
            self.since_change = Instant::now();
            return Some(PetState { activity, detail });
        }
        None
    }

    /// 消费一条 rollout 事件，返回需要推给前端的新状态。
    fn on_event(&mut self, ev: &Value) -> Option<PetState> {
        let outer = ev.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = ev.get("payload").cloned().unwrap_or(Value::Null);
        let inner = payload.get("type").and_then(Value::as_str).unwrap_or("");

        match (outer, inner) {
            ("event_msg", "task_started") => {
                self.pending_tools = 0;
                self.set(Activity::Thinking, "turn started")
            }
            ("event_msg", "token_count") => {
                if self.pending_tools > 0 {
                    self.set(Activity::Tool, "tool running")
                } else {
                    self.set(Activity::Writing, "generating")
                }
            }
            ("event_msg", "item_completed") => {
                let kind = payload.get("item").and_then(|i| i.get("type")).and_then(Value::as_str).unwrap_or("");
                match kind {
                    "AgentMessage" => self.set(Activity::Writing, "message done"),
                    "UserMessage" => None,
                    _ => None,
                }
            }
            ("event_msg", "task_complete") => {
                self.pending_tools = 0;
                self.set(Activity::Done, "turn complete")
            }
            ("event_msg", "turn_aborted") => {
                self.pending_tools = 0;
                self.set(Activity::Error, "turn aborted")
            }
            ("response_item", "function_call") | ("response_item", "custom_tool_call")
            | ("response_item", "web_search_call") => {
                self.pending_tools += 1;
                self.set(Activity::Tool, "tool call")
            }
            ("response_item", "function_call_output") | ("response_item", "custom_tool_call_output") => {
                self.pending_tools = (self.pending_tools - 1).max(0);
                if self.pending_tools == 0 {
                    self.set(Activity::Thinking, "tool done")
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// 时间驱动：长时间没事件就回 idle / 睡着，done/error 一闪而过。
    fn tick(&mut self) -> Option<PetState> {
        let idle_for = self.last_event.elapsed();
        match self.activity {
            Activity::Done | Activity::Error if self.since_change.elapsed() > FLASH => {
                self.set(Activity::Idle, "settled")
            }
            Activity::Thinking | Activity::Writing | Activity::Tool if idle_for > IDLE_AFTER => {
                self.pending_tools = 0;
                self.set(Activity::Idle, "idle")
            }
            Activity::Idle if idle_for > SLEEP_AFTER => self.set(Activity::Sleeping, "sleeping"),
            Activity::Sleeping if idle_for < SLEEP_AFTER => self.set(Activity::Idle, "woke up"),
            _ => None,
        }
    }
}

struct Tailer {
    path: Option<PathBuf>,
    pos: u64,
    attached: bool,
}

impl Tailer {
    fn new() -> Self {
        Self { path: None, pos: 0, attached: false }
    }

    fn poll(&mut self, root: &Path) -> Vec<Value> {
        let newest = newest_session(root);
        if newest != self.path {
            self.path = newest;
            // 首次挂上已有文件时直接从末尾开始，避免把上一次会话的历史重播一遍
            self.pos = if self.attached {
                0
            } else {
                self.path.as_ref().and_then(|p| fs::metadata(p).ok()).map(|m| m.len()).unwrap_or(0)
            };
            self.attached = true;
        }
        let Some(path) = self.path.clone() else { return Vec::new() };
        let Ok(mut file) = fs::File::open(&path) else { return Vec::new() };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.pos {
            self.pos = 0;
        }
        if len == self.pos {
            return Vec::new();
        }
        if file.seek(SeekFrom::Start(self.pos)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if file.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        // 只消费到最后一个换行，避免把半行 JSON 吃掉
        let Some(cut) = buf.iter().rposition(|b| *b == b'\n') else { return Vec::new() };
        self.pos += (cut + 1) as u64;
        String::from_utf8_lossy(&buf[..cut])
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    }
}

fn newest_session(root: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let modified = entry.metadata().and_then(|m| m.modified()).ok();
            if let Some(modified) = modified {
                if best.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
                    best = Some((modified, path));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

fn sessions_root() -> PathBuf {
    if let Ok(custom) = std::env::var("CODEX_PET_SESSIONS") {
        return PathBuf::from(custom);
    }
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\yan".into());
    PathBuf::from(home).join(".codex").join("sessions")
}

/// 统一的推送出口：改状态 + 发事件，tail / HTTP / chat 三条入口共用。
fn emit_state(app: &tauri::AppHandle, machine: &Shared, activity: Activity, detail: &str) {
    let changed = {
        let mut m = machine.lock().unwrap();
        m.set(activity, detail)
    };
    if let Some(state) = changed {
        eprintln!("[codex-pet] -> {:?} ({})", state.activity, state.detail);
        let _ = app.emit("pet:state", state);
    }
}

#[derive(Clone, Serialize)]
struct Bubble {
    text: String,
    done: bool,
}

#[derive(Deserialize, Clone)]
struct ChatConfig {
    /// 命令模板，`{prompt}` 会被替换成输入文本
    command: Vec<String>,
    #[serde(default)]
    cwd: String,
    /// 额外环境变量，比如 HTTPS_PROXY
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    chat: Option<ChatConfig>,
    /// 语音朗读开关，右键菜单切换后写回 pet.json
    #[serde(default)]
    voice: Option<bool>,
    /// 语音合成配置
    #[serde(default)]
    tts: Option<TtsConfig>,
    /// 语音输入（说话）配置
    #[serde(default)]
    asr: Option<AsrConfig>,
    /// 宠物缩放，0.8 / 1.0 / 1.2
    #[serde(default)]
    scale: Option<f64>,
    /// 窗口总在最前
    #[serde(default)]
    always_on_top: Option<bool>,
}

/// 语音输入：ffmpeg 录麦克风，交给识别服务转文字。
/// 识别服务可切换：whisper.cpp（本地）/ OpenAI 兼容 / 自定义命令。
#[derive(Deserialize, Default, Clone)]
struct AsrConfig {
    /// `whisper`（默认）/ `openai` / `command`
    #[serde(default)]
    provider: Option<String>,
    /// whisper.cpp server 的转写地址，默认 http://127.0.0.1:8080/inference
    #[serde(default)]
    endpoint: Option<String>,
    /// OpenAI 兼容的转写地址，默认 https://api.openai.com/v1/audio/transcriptions
    #[serde(default)]
    api_endpoint: Option<String>,
    /// ffmpeg 路径，留空自动找（exe 同目录 / 上层 tools 目录 / PATH）
    #[serde(default)]
    ffmpeg: Option<String>,
    /// 识别语言，默认 zh
    #[serde(default)]
    language: Option<String>,
    /// dshow 录音设备名，留空自动挑第一个麦克风
    #[serde(default)]
    device: Option<String>,
    /// API key（openai 模式用），也可以走环境变量
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    /// 模型名（openai 模式用）
    #[serde(default)]
    model: Option<String>,
    /// 外部命令模式：命令里可用 {audio_file} {language} 占位
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    proxy: Option<String>,
}

/// 语音合成配置。供应商可切换，字段全在 tts.rs 里定义。
type TtsConfig = tts::TtsSettings;

/// 语音朗读开关。跨线程共享，MCP / HTTP / 菜单三条路都读它。
static VOICE: AtomicBool = AtomicBool::new(false);
static TTS_SEQ: AtomicU32 = AtomicU32::new(0);

fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("PET_CONFIG") {
        return PathBuf::from(p);
    }
    // 先找可执行文件同目录，再退到 %APPDATA%\codex-pet\pet.json
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        let local = dir.join("pet.json");
        if local.exists() {
            return local;
        }
    }
    if cfg!(debug_assertions) {
        let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("pet.json");
        if dev.exists() {
            return dev;
        }
    }
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(appdata).join("codex-pet").join("pet.json")
}

/// 读配置。文件不存在或坏掉时，字段级别的默认值要和 `default_config_value()` 一致——
/// 否则设置界面显示的出厂值和实际行为会对不上（语音开关曾经就是这样不一致的）。
fn load_config() -> Config {
    let raw = fs::read_to_string(config_path()).ok();
    let mut cfg: Config = raw
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    if cfg.voice.is_none() {
        cfg.voice = Some(true);
    }
    if cfg.scale.is_none() {
        cfg.scale = Some(1.0);
    }
    if cfg.always_on_top.is_none() {
        cfg.always_on_top = Some(true);
    }
    if cfg.chat.is_none() {
        cfg.chat = Some(default_chat());
    }
    cfg
}

fn default_chat() -> ChatConfig {
    ChatConfig {
        command: ["codex", "exec", "--json", "--skip-git-repo-check", "{prompt}"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        cwd: String::new(),
        env: Default::default(),
    }
}

/// 出厂默认配置：设置界面的「恢复默认」拿它填表单，配置文件丢了/坏了也拿它兜底。
fn default_config_value() -> Value {
    serde_json::json!({
        "port": DEFAULT_PORT,
        "voice": true,
        "chat": {
            "command": ["codex", "exec", "--json", "--skip-git-repo-check", "{prompt}"],
            "cwd": "",
            "env": {}
        },
        "tts": {
            "provider": "minimax",
            "endpoint": "https://api.minimaxi.com",
            "path": "/v1/t2a_v2",
            "model": "speech-2.8-turbo",
            "voice": "female-tianmei",
            "api_key_env": "MINIMAX_API_KEY",
            "audio_path": "data.audio",
            "audio_format": "hex",
            "sapi_voice": "",
            "proxy": ""
        },
        "asr": {
            "provider": "whisper",
            "endpoint": "http://127.0.0.1:8080/inference",
            "api_endpoint": "https://api.openai.com/v1/audio/transcriptions",
            "ffmpeg": "",
            "language": "zh",
            "device": ""
        },
        "scale": 1.0,
        "always_on_top": true
    })
}

/// 端口发现文件的路径：`--mcp` 进程和外部适配器都从这里读真实端口。
fn endpoint_path() -> PathBuf {
    if let Ok(p) = std::env::var("PET_ENDPOINT") {
        return PathBuf::from(p);
    }
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(appdata).join("codex-pet").join("endpoint.json")
}

fn write_endpoint(port: u16) {
    let path = endpoint_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let body = serde_json::json!({ "port": port, "pid": std::process::id() }).to_string();
    if let Err(e) = fs::write(&path, body) {
        eprintln!("[codex-pet] 写 {} 失败：{e}", path.display());
    }
}

/// 从 agent 的一行输出里抽可显示文本。
/// 认识 `codex exec --json` 的 `item.completed/agent_message`，也接受通用的 text/message 字段；
/// 不是 JSON 的行按纯文本接受——这样任何 CLI agent 都能直接接。
fn extract_text(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Some(line.to_string());
    };
    if let Some(item) = v.get("item") {
        if item.get("type").and_then(Value::as_str) == Some("agent_message") {
            if let Some(t) = item.get("text").and_then(Value::as_str) {
                return Some(t.to_string());
            }
        }
    }
    for key in ["text", "message", "content", "delta"] {
        if let Some(t) = v.get(key).and_then(Value::as_str) {
            if !t.trim().is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// 统一的出气泡口：所有气泡都走这里，开着语音朗读时自动念出来。
/// `done` 的那一次才会念——流式中间态只上屏不出声。
fn emit_bubble(app: &tauri::AppHandle, text: String, done: bool) {
    if done && VOICE.load(Ordering::Relaxed) {
        speak(app, text.clone());
    }
    let _ = app.emit("pet:bubble", Bubble { text, done });
}

/// 朗读一段文本。供应商由配置决定（见 tts.rs 的预设表），
/// 念的时候通知前端切到「说话」姿态，念完再切回来。
fn speak(app: &tauri::AppHandle, text: String) {
    let clean: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("agent 没有输出文本"))
        .collect::<Vec<_>>()
        .join(" ");
    let clean = clean.trim().to_string();
    if clean.is_empty() {
        return;
    }
    // 太长的回复只念开头，免得一次读几分钟
    let spoken: String = if clean.chars().count() > 220 {
        clean.chars().take(220).collect::<String>() + "……"
    } else {
        clean
    };

    let cfg = load_config();
    let tts = cfg.tts.unwrap_or_default();

    // 朗读是串行的事，别堵住调用方线程
    let handle = app.clone();
    thread::spawn(move || {
        let _ = handle.emit("pet:speaking", serde_json::json!({ "on": true }));
        let result = speak_sync(&tts, &spoken);
        let _ = handle.emit("pet:speaking", serde_json::json!({ "on": false }));
        if let Err(e) = result {
            eprintln!("[codex-pet] 朗读失败：{e}");
        }
    });
}

/// 同步朗读。出错把原因写到 stderr 和临时错误文件，界面上也会显示。
fn speak_sync(cfg: &TtsConfig, text: &str) -> Result<(), String> {
    let err_file = std::env::temp_dir().join("codex-pet-tts.err");
    let _ = fs::remove_file(&err_file);

    // 本机 SAPI 是独立的一条路（不走云端）
    if cfg.provider_id() == "sapi" {
        return sapi_speak(cfg, text);
    }

    let seq = TTS_SEQ.fetch_add(1, Ordering::Relaxed);
    let audio = tts::synthesize(cfg, text).map_err(|e| {
        let _ = fs::write(&err_file, format!("{e}\n"));
        e
    })?;
    let tag = format!("{}-{seq}", std::process::id());
    tts::play(&audio, &tag).map_err(|e| {
        let _ = fs::write(&err_file, format!("{e}\n"));
        e
    })
}

/// 本机 SAPI 朗读。音色按「中文女声 → 任意中文 → 任意女声」挑，
/// 一个女声都没有就静默跳过——宁可不出声，也不要冒出男声。
fn sapi_speak(cfg: &TtsConfig, text: &str) -> Result<(), String> {
    const SAPI_SCRIPT: &str = r#"param(
  [Parameter(Mandatory=$true)][string]$TextFile,
  [string]$SapiVoice = ""
)
$ErrorActionPreference = "Continue"
$text = [System.IO.File]::ReadAllText($TextFile, [System.Text.Encoding]::UTF8)
if (-not $text -or -not $text.Trim()) { exit 0 }

$errFile = Join-Path $env:TEMP "codex-pet-tts.err"
Add-Type -AssemblyName System.Speech
$s = New-Object System.Speech.Synthesis.SpeechSynthesizer
$picked = ""
try {
  if ($SapiVoice) {
    $s.SelectVoice($SapiVoice)
    $picked = $SapiVoice
  } else {
    $voices = @($s.GetInstalledVoices() | Where-Object { $_.Enabled } | ForEach-Object { $_.VoiceInfo })
    $target = $voices | Where-Object { $_.Culture.Name -like 'zh*' -and $_.Gender -eq 'Female' } | Select-Object -First 1
    if (-not $target) { $target = $voices | Where-Object { $_.Culture.Name -like 'zh*' } | Select-Object -First 1 }
    if (-not $target) { $target = $voices | Where-Object { $_.Gender -eq 'Female' } | Select-Object -First 1 }
    if ($target) {
      $s.SelectVoice($target.Name)
      $picked = $target.Name
    }
  }
} catch { }
if (-not $picked) {
  try { [System.IO.File]::WriteAllText($errFile, "sapi: 本机没有可用的女声，跳过朗读（请在系统里安装中文女声）") } catch { }
  exit 0
}
$s.Speak($text)
"#;

    let script = std::env::temp_dir().join("codex-pet-sapi.ps1");
    let current = fs::read_to_string(&script).unwrap_or_default();
    // 必须带 BOM 写入：PowerShell 5.1 对无 BOM 的脚本按 ANSI 解码，中文注释会吞掉
    // 后面的行（实测把下一行的赋值整行变成注释，脚本静默 exit 0，一点声音都没有）
    if current.trim_start_matches('\u{feff}') != SAPI_SCRIPT {
        fs::write(&script, format!("\u{feff}{SAPI_SCRIPT}"))
            .map_err(|e| format!("写 SAPI 脚本失败：{e}"))?;
    }

    let seq = TTS_SEQ.fetch_add(1, Ordering::Relaxed);
    let text_file = std::env::temp_dir().join(format!("codex-pet-sapi-{}-{seq}.txt", std::process::id()));
    fs::write(&text_file, text.as_bytes()).map_err(|e| format!("写临时文件失败：{e}"))?;

    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ])
    .arg(&script)
    .arg("-TextFile")
    .arg(&text_file);
    if let Some(v) = cfg.sapi_voice.as_deref() {
        if !v.trim().is_empty() {
            cmd.arg("-SapiVoice").arg(v);
        }
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| format!("调 PowerShell 失败：{e}"))?;
    let _ = fs::remove_file(&text_file);
    if !out.status.success() {
        let err = std::env::temp_dir().join("codex-pet-tts.err");
        let msg = fs::read_to_string(&err).unwrap_or_default();
        return Err(if msg.trim().is_empty() {
            "本机朗读失败".into()
        } else {
            msg.trim().to_string()
        });
    }
    Ok(())
}

/// 跑一次对话：执行配置里的命令，逐行抽文本推气泡（流式），结束补 done。
fn run_chat(app: tauri::AppHandle, machine: Shared, text: String) {
    let cfg = load_config();
    let chat = cfg.chat.unwrap_or_else(default_chat);
    emit_state(&app, &machine, Activity::Thinking, "asking agent");

    let mut args: Vec<String> = chat.command.iter().map(|a| a.replace("{prompt}", &text)).collect();
    if args.is_empty() {
        emit_bubble(&app, "pet.json 里 chat.command 是空的".into(), true);
        emit_state(&app, &machine, Activity::Error, "no command");
        return;
    }
    let program = args.remove(0);
    let mut cmd = Command::new(&program);
    cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::piped());
    if !chat.cwd.is_empty() {
        cmd.current_dir(&chat.cwd);
    }
    for (k, v) in &chat.env {
        cmd.env(k, v);
    }
    // stderr 落文件：失败时把最后几行贴进气泡，免得只能看“没有可显示的回复”
    let err_path = std::env::temp_dir().join("codex-pet-agent.err");
    if let Ok(f) = fs::File::create(&err_path) {
        cmd.stderr(Stdio::from(f));
    } else {
        cmd.stderr(Stdio::null());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：别弹出 cmd
    }

    match cmd.spawn() {
        Ok(mut child) => {
            let mut acc = String::new();
            if let Some(stdout) = child.stdout.take() {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Some(piece) = extract_text(&line) {
                        if !acc.is_empty() && !acc.ends_with('\n') {
                            acc.push('\n');
                        }
                        acc.push_str(&piece);
                        emit_bubble(&app, acc.clone(), false);
                    }
                }
            }
            let status = child.wait();
            if acc.is_empty() {
                let err = fs::read_to_string(&err_path).unwrap_or_default();
                let lines: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).collect();
                let tail = lines[lines.len().saturating_sub(6)..].join("\n");
                let msg = if tail.is_empty() {
                    "（没有可显示的回复）".to_string()
                } else {
                    format!("agent 没有输出文本，stderr：\n{tail}")
                };
                acc.push_str(&msg);
            }
            emit_bubble(&app, acc, true);
            match status {
                Ok(s) if s.success() => emit_state(&app, &machine, Activity::Done, "reply ready"),
                _ => emit_state(&app, &machine, Activity::Error, "agent failed"),
            }
        }
        Err(e) => {
            emit_bubble(&app, format!("启动 {program} 失败：{e}"), true);
            emit_state(&app, &machine, Activity::Error, "chat failed");
        }
    }
}

#[tauri::command]
fn pet_chat(app: tauri::AppHandle, machine: tauri::State<'_, Shared>, text: String) -> Result<(), String> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("empty prompt".into());
    }
    let shared = machine.inner().clone();
    thread::spawn(move || run_chat(app, shared, text));
    Ok(())
}

/// 把单个字段写回 pet.json（保留其它字段与顺序之外的格式不重要）。
fn patch_config(key: &str, value: Value) {
    let path = config_path();
    let mut v: Value = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(default_config_value);
    if !v.is_object() {
        v = default_config_value();
    }
    if let Some(obj) = v.as_object_mut() {
        obj.insert(key.to_string(), value);
    }
    write_config(&path, &v);
}

fn write_config(path: &Path, value: &Value) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    match serde_json::to_string_pretty(value) {
        Ok(s) => {
            // 先写临时文件再改名，避免写到一半断电/崩溃把配置留成半截
            let tmp = path.with_extension("json.tmp");
            if fs::write(&tmp, s).and_then(|_| fs::rename(&tmp, path)).is_err() {
                eprintln!("[codex-pet] 写 {} 失败", path.display());
                let _ = fs::remove_file(&tmp);
            }
        }
        Err(e) => eprintln!("[codex-pet] 序列化配置失败：{e}"),
    }
}

fn save_voice(enabled: bool) {
    patch_config("voice", Value::Bool(enabled));
}

const BASE_W: f64 = 320.0;
const BASE_H: f64 = 380.0;

fn apply_scale(app: &tauri::AppHandle, scale: f64) {
    let s = if scale.is_finite() { scale.clamp(0.7, 1.5) } else { 1.0 };
    if let Some(win) = app.get_webview_window("pet") {
        let _ = win.set_size(tauri::LogicalSize::new(BASE_W * s, BASE_H * s));
    }
    let _ = app.emit("pet:scale", s);
}

#[tauri::command]
fn pet_ui_state() -> Value {
    let cfg = load_config();
    serde_json::json!({
        // 语音开关读配置文件而不是内存原子量：前端可能在 setup 之前就来问，
        // 那时 VOICE 还是默认的 false，会显示成「没开语音」
        "voice": cfg.voice.unwrap_or(true),
        "config": config_path().to_string_lossy(),
        "scale": cfg.scale.unwrap_or(1.0),
        "alwaysOnTop": cfg.always_on_top.unwrap_or(true),
    })
}

#[tauri::command]
fn pet_set_voice(enabled: bool) -> Result<(), String> {
    VOICE.store(enabled, Ordering::Relaxed);
    save_voice(enabled);
    Ok(())
}

/// 界面不回显密钥，提交里 key 留空时沿用磁盘上原来那份；
/// 填了新值就以新值为准。
fn merge_secret(next: &mut Value, old: Option<&Value>, section: &str) {
    let Some(old_key) = old
        .and_then(|o| o.get(section))
        .and_then(|s| s.get("api_key"))
        .cloned()
    else {
        return;
    };
    let Some(obj) = next.get_mut(section).and_then(Value::as_object_mut) else {
        return;
    };
    let blank = obj
        .get("api_key")
        .and_then(Value::as_str)
        .map(|v| v.trim().is_empty())
        .unwrap_or(true);
    if blank {
        obj.insert("api_key".to_string(), old_key);
    }
}

/// 设置界面用：回显配置（去掉 API key，密钥不回显也不落表单）＋出厂默认值。
#[tauri::command]
fn pet_get_config() -> Value {
    let path = config_path();
    let mut cfg: Value = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(Value::is_object)
        .unwrap_or_else(default_config_value);
    if let Some(tts) = cfg.get_mut("tts").and_then(Value::as_object_mut) {
        tts.remove("api_key");
    }
    if let Some(asr) = cfg.get_mut("asr").and_then(Value::as_object_mut) {
        asr.remove("api_key");
    }
    serde_json::json!({
        "config": cfg,
        "defaults": default_config_value(),
        "path": path.to_string_lossy(),
    })
}

/// 保存设置界面提交的整份配置。界面只改写自己认识的字段，其它键原样带回。
#[tauri::command]
fn pet_save_config(app: tauri::AppHandle, config: Value) -> Result<String, String> {
    if !config.is_object() {
        return Err("配置必须是 JSON 对象".into());
    }
    let path = config_path();
    let mut next = config;
    // 界面不回显密钥，保存时把磁盘上原有的密钥保回来——
    // 填了新值就覆盖，留空就继续用原来那份。
    let old: Option<Value> = fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok());
    for section in ["tts", "asr"] {
        merge_secret(&mut next, old.as_ref(), section);
    }
    write_config(&path, &next);

    if let Some(v) = next.get("voice").and_then(Value::as_bool) {
        VOICE.store(v, Ordering::Relaxed);
    }
    if let Some(s) = next.get("scale").and_then(Value::as_f64) {
        apply_scale(&app, s);
    }
    if let Some(b) = next.get("always_on_top").and_then(Value::as_bool) {
        if let Some(win) = app.get_webview_window("pet") {
            let _ = win.set_always_on_top(b);
        }
    }
    Ok(format!("已保存到 {}", path.display()))
}

/// 设置界面用：拿到全部 TTS 供应商预设（渲染下拉框用）。
#[tauri::command]
fn pet_tts_providers() -> Value {
    serde_json::json!({ "providers": tts::provider_list() })
}

/// 试听。失败把原因回传给界面显示（成功就返回空串）。
/// `config` 传了就用界面上的当前值，没传就用磁盘上的——这样改完没保存也能试。
#[tauri::command]
fn pet_preview_voice(config: Option<Value>, text: Option<String>) -> Result<(), String> {
    let t = text
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "你好呀，我是 alice，很高兴见到你。".to_string());
    let tts = match config {
        Some(c) => {
            let mut t = c.get("tts").cloned().unwrap_or(Value::Null);
            // 界面不回显密钥：用户没重新填就沿用磁盘上保存的那份，
            // 否则改个音色想试听，反而因为 key 空了试听失败。
            let disk = serde_json::json!({ "tts": { "api_key": load_config().tts.as_ref().and_then(|t| t.api_key.clone()) } });
            merge_secret(&mut t, Some(&disk), "tts");
            serde_json::from_value::<TtsConfig>(t).unwrap_or_default()
        }
        None => load_config().tts.unwrap_or_default(),
    };
    speak_sync(&tts, &t)
}

#[tauri::command]
fn pet_open_settings(app: tauri::AppHandle) -> Result<(), String> {
    let win = app.get_webview_window("settings").ok_or("设置窗口没有创建")?;
    win.show().map_err(|e| e.to_string())?;
    win.set_focus().map_err(|e| e.to_string())?;
    let _ = app.emit_to("settings", "settings:refresh", ());
    Ok(())
}

#[tauri::command]
fn pet_set_topmost(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("pet") {
        win.set_always_on_top(enabled).map_err(|e| e.to_string())?;
    }
    patch_config("always_on_top", Value::Bool(enabled));
    Ok(())
}

#[tauri::command]
fn pet_set_scale(app: tauri::AppHandle, scale: f64) -> Result<(), String> {
    apply_scale(&app, scale);
    patch_config("scale", serde_json::json!(scale.clamp(0.7, 1.5)));
    Ok(())
}

#[tauri::command]
fn pet_hide_window(app: tauri::AppHandle) {
    // 真的隐藏窗口，否则透明区域仍会挡住鼠标；从托盘或 Ctrl+Alt+P 唤回
    if let Some(win) = app.get_webview_window("pet") {
        let _ = win.hide();
    }
}

#[tauri::command]
fn pet_quit(app: tauri::AppHandle) {
    app.exit(0);
}

// ---------- 语音输入：ffmpeg 录麦克风 → 本机 whisper-server 转文字 ----------

struct Recorder {
    child: Child,
    wav: PathBuf,
}

static REC: Mutex<Option<Recorder>> = Mutex::new(None);
static FFMPEG_DEV: Mutex<Option<String>> = Mutex::new(None);

/// 找 ffmpeg：配置 > PET_FFMPEG 环境变量 > exe 同目录 > 上层 tools 目录 > PATH。
fn ffmpeg_path(cfg: &Config) -> String {
    if let Some(p) = cfg.asr.as_ref().and_then(|a| a.ffmpeg.clone()).filter(|s| !s.trim().is_empty()) {
        return p;
    }
    if let Ok(p) = std::env::var("PET_FFMPEG") {
        if !p.trim().is_empty() {
            return p;
        }
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        let mut cur = Some(dir);
        // exe 在 src-tauri/target/release，往上找几层就能碰到仓库根的 tools/ffmpeg.exe
        for _ in 0..5 {
            let Some(d) = cur else { break };
            let candidate = d.join("ffmpeg.exe");
            if candidate.exists() {
                return candidate.to_string_lossy().to_string();
            }
            let tools = d.join("tools").join("ffmpeg.exe");
            if tools.exists() {
                return tools.to_string_lossy().to_string();
            }
            cur = d.parent().map(Path::to_path_buf);
        }
    }
    "ffmpeg".to_string()
}

/// 从 `ffmpeg -list_devices` 里挑第一个 (audio) 设备名，结果缓存。
fn dshow_audio_device(ffmpeg: &str) -> Option<String> {
    if let Some(cached) = FFMPEG_DEV.lock().unwrap().clone() {
        return Some(cached);
    }
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"])
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    let device = text.lines().find_map(|line| {
        let line = line.trim();
        if !line.ends_with("(audio)") {
            return None;
        }
        let start = line.find('"')? + 1;
        let rest = &line[start..];
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    })?;
    *FFMPEG_DEV.lock().unwrap() = Some(device.clone());
    Some(device)
}

/// 静音时 whisper 会吐「(字幕:...)」「[音乐]」这类幻听，过滤掉按没听清处理。
fn clean_asr(raw: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    for line in raw.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        let bracket = l.starts_with('(') || l.starts_with('（') || l.starts_with('[') || l.starts_with('【');
        let noise = ["字幕", "音乐", "掌声", "Music", "Applause", "BLANK", "笑声", "silence"]
            .iter()
            .any(|k| l.contains(k));
        if bracket && noise {
            continue;
        }
        kept.push(l);
    }
    kept.join(" ").trim().to_string()
}

/// whisper 在静音上会稳定幻听出这几句完整短语，整句命中就当没听见。
fn is_asr_hallucination(text: &str) -> bool {
    let norm: String = text
        .chars()
        .filter(|c| c.is_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(c))
        .collect::<String>()
        .to_lowercase();
    const PHRASES: [&str; 12] = [
        "谢谢大家收看",
        "謝謝大家收看",
        "谢谢大家",
        "謝謝大家",
        "谢谢观看",
        "謝謝觀看",
        "请不吝点赞订阅",
        "字幕由amaraorg社区提供",
        "字幕志愿者",
        "明镜与点点栏目",
        "thankyou",
        "thankyouforwatching",
    ];
    PHRASES.iter().any(|p| norm == *p)
}

/// 识别一段录音。引擎由 asr.provider 决定。
fn transcribe(wav: &Path, cfg: &Config) -> Result<String, String> {
    let asr = cfg.asr.clone().unwrap_or_default();
    let provider = asr.provider.clone().unwrap_or_else(|| "whisper".to_string());
    let raw = match provider.as_str() {
        "command" => asr_command(wav, &asr)?,
        "openai" => asr_openai(wav, &asr)?,
        _ => asr_whisper(wav, &asr)?,
    };
    let text = clean_asr(&raw);
    Ok(if is_asr_hallucination(&text) { String::new() } else { text })
}

/// 本机 whisper.cpp server。multipart 上传，响应是 {"text": "..."}。
fn asr_whisper(wav: &Path, asr: &AsrConfig) -> Result<String, String> {
    let endpoint = asr
        .endpoint
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8080/inference".to_string());
    let lang = asr
        .language
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "zh".to_string());

    let mut cmd = Command::new("curl");
    cmd.args([
        "-s",
        "-m",
        "90",
        "-F",
        &format!("file=@{}", wav.display()),
        "-F",
        "response_format=json",
        "-F",
        &format!("language={lang}"),
    ])
    .arg(&endpoint);
    if let Some(p) = asr.proxy.as_deref().filter(|s| !s.trim().is_empty()) {
        cmd.arg("-x").arg(p);
    }
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| format!("调 curl 失败：{e}"))?;
    if !out.status.success() {
        return Err(format!("识别服务不可用（{endpoint}）"));
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|_| "识别服务返回的不是 JSON".to_string())?;
    if let Some(err) = v.get("error").and_then(Value::as_str) {
        return Err(format!("识别失败：{err}"));
    }
    Ok(v.get("text").and_then(Value::as_str).unwrap_or("").to_string())
}

/// OpenAI 兼容的转写地址。只认 `api_endpoint`——
/// `endpoint` 是本机 whisper 的地址，拿来当云端转写地址会打到错误的服务上。
fn openai_endpoint(asr: &AsrConfig) -> String {
    asr.api_endpoint
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://api.openai.com/v1/audio/transcriptions".to_string())
}

/// OpenAI 兼容的 /audio/transcriptions。multipart 上传，响应也是 {"text": "..."}。
fn asr_openai(wav: &Path, asr: &AsrConfig) -> Result<String, String> {
    let endpoint = openai_endpoint(asr);
    let lang = asr
        .language
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "zh".to_string());
    let model = asr
        .model
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "whisper-1".to_string());

    let key_env = asr
        .api_key_env
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "OPENAI_API_KEY".to_string());
    let key = tts::lookup_key(
        asr.api_key.as_deref().unwrap_or_default(),
        &key_env,
    );
    if key.is_empty() {
        return Err(format!("没拿到 API key：填在设置里，或设环境变量 {key_env}"));
    }

    // key 通过 stdin 交给 curl（curl -K - 从 stdin 读配置）：
    // 既不在进程命令行里（ps 能看到），也不会落盘成明文文件。
    let curl_cfg = format!("header = \"Authorization: Bearer {}\"\n", key.replace('"', "\\\""));

    let mut cmd = Command::new("curl");
    cmd.args([
        "-s",
        "-m",
        "120",
        "-K",
        "-",
        "-F",
        &format!("file=@{}", wav.display()),
        "-F",
        "response_format=json",
        "-F",
        &format!("language={lang}"),
        "-F",
        &format!("model={model}"),
    ])
    .arg(&endpoint);
    if let Some(p) = asr.proxy.as_deref().filter(|s| !s.trim().is_empty()) {
        cmd.arg("-x").arg(p);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("调 curl 失败：{e}"))?;
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(curl_cfg.as_bytes());
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("等 curl 结束失败：{e}"))?;
    if !out.status.success() {
        let body = String::from_utf8_lossy(&out.stdout);
        let err = String::from_utf8_lossy(&out.stderr);
        let detail = if body.trim().is_empty() {
            err.lines().next().unwrap_or("").trim().to_string()
        } else {
            body.chars().take(200).collect::<String>()
        };
        return Err(format!("识别服务返回错误（{}）：{detail}", out.status));
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|_| "识别服务返回的不是 JSON".to_string())?;
    if let Some(err) = v.get("error") {
        let d = err
            .get("message")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .unwrap_or_else(|| err.to_string());
        return Err(format!("识别失败：{d}"));
    }
    Ok(v.get("text").and_then(Value::as_str).unwrap_or("").to_string())
}

/// 外部命令模式：命令可用 {audio_file} {language} 占位，stdout 就是识别结果。
fn asr_command(wav: &Path, asr: &AsrConfig) -> Result<String, String> {
    let cmdline = asr
        .command
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or("外部命令模式还没填命令")?;
    let lang = asr.language.clone().unwrap_or_else(|| "zh".to_string());
    let expanded = cmdline
        .replace("{audio_file}", &wav.to_string_lossy())
        .replace("{language}", &lang);

    let mut cmd = Command::new("cmd");
    cmd.args(["/C", &expanded]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| format!("跑外部命令失败：{e}"))?;
    if !out.status.success() {
        let err = decode_console_output(&out.stderr);
        return Err(format!(
            "外部命令失败：{}",
            err.lines().next().unwrap_or("").trim()
        ));
    }
    Ok(decode_console_output(&out.stdout).trim().to_string())
}

/// 命令输出解码。Windows 控制台输出重定向到管道时用的是 ANSI 代码页（936/GBK），
/// 不是 UTF-8——chcp 在这种场景下不生效，只能在读出来之后按 GBK 解。
pub fn decode_console_output(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => encoding_rs::GBK.decode(bytes).0.into_owned(),
    }
}

#[tauri::command]
fn pet_voice_start(app: tauri::AppHandle) -> Result<(), String> {
    if REC.lock().unwrap().is_some() {
        return Err("正在录音".into());
    }
    let cfg = load_config();
    let ffmpeg = ffmpeg_path(&cfg);
    let device = cfg
        .asr
        .as_ref()
        .and_then(|a| a.device.clone())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| dshow_audio_device(&ffmpeg))
        .ok_or_else(|| format!("没找到麦克风（{ffmpeg} -list_devices 没返回 audio 设备）"))?;

    let wav = std::env::temp_dir().join(format!("codex-pet-rec-{}.wav", std::process::id()));
    let _ = fs::remove_file(&wav);

    let mut cmd = Command::new(&ffmpeg);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "dshow",
        "-i",
        &format!("audio={device}"),
        "-t",
        "60",
        "-ar",
        "16000",
        "-ac",
        "1",
        "-c:a",
        "pcm_s16le",
        "-y",
    ])
    .arg(&wav)
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let child = cmd.spawn().map_err(|e| format!("启动 ffmpeg 失败：{e}"))?;
    *REC.lock().unwrap() = Some(Recorder { child, wav: wav.clone() });
    let _ = app.emit("pet:listening", serde_json::json!({ "on": true }));
    Ok(())
}

#[tauri::command]
fn pet_voice_stop(app: tauri::AppHandle) -> Result<String, String> {
    let Some(mut rec) = REC.lock().unwrap().take() else {
        return Err("没有在录音".into());
    };
    // 给 ffmpeg 的 stdin 发 q，它才会把 WAV 文件头写完整再退出
    if let Some(mut stdin) = rec.child.stdin.take() {
        let _ = stdin.write_all(b"q\n");
        let _ = stdin.flush();
    }
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        match rec.child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(80)),
            _ => {
                let _ = rec.child.kill();
                let _ = rec.child.wait();
                break;
            }
        }
    }
    let _ = app.emit("pet:listening", serde_json::json!({ "on": false }));

    let size = fs::metadata(&rec.wav).map(|m| m.len()).unwrap_or(0);
    if size < 3000 {
        let _ = fs::remove_file(&rec.wav);
        return Err("录音太短，没听清".into());
    }
    let cfg = load_config();
    let text = transcribe(&rec.wav, &cfg);
    let _ = fs::remove_file(&rec.wav);
    let text = text?;
    if text.is_empty() {
        return Err("没听清，再说一次？".into());
    }
    Ok(text)
}

#[tauri::command]
fn pet_voice_cancel(app: tauri::AppHandle) {
    if let Some(mut rec) = REC.lock().unwrap().take() {
        let _ = rec.child.kill();
        let _ = rec.child.wait();
        let _ = fs::remove_file(&rec.wav);
    }
    let _ = app.emit("pet:listening", serde_json::json!({ "on": false }));
}

fn parse_activity(s: &str) -> Activity {
    match s {
        "idle" => Activity::Idle,
        "sleeping" => Activity::Sleeping,
        "thinking" => Activity::Thinking,
        "writing" => Activity::Writing,
        "tool" => Activity::Tool,
        "done" => Activity::Done,
        "error" => Activity::Error,
        _ => Activity::Thinking,
    }
}

fn json_body(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

/// 本地 HTTP：任何 agent 都能把状态/气泡推进来，并触发一次对话。
/// POST /event {"activity":"tool","detail":"Bash"}
/// POST /say   {"text":"你好"}
/// POST /chat  {"text":"帮我看看这个文件"}
/// GET  /state
fn configure_http(app: &tauri::AppHandle, machine: Shared) {
    let cfg = load_config();
    let preferred = cfg
        .port
        .or_else(|| std::env::var("PET_PORT").ok().and_then(|p| p.parse().ok()));
    let handle = app.clone();
    thread::spawn(move || {
        // 端口被占就往后顺延，别让用户自己去查“为什么推不动”
        let mut candidates: Vec<u16> = preferred.into_iter().collect();
        for p in DEFAULT_PORT..DEFAULT_PORT + 10 {
            if !candidates.contains(&p) {
                candidates.push(p);
            }
        }
        let mut bound = None;
        for p in candidates {
            let addr = format!("127.0.0.1:{p}");
            match tiny_http::Server::http(&addr) {
                Ok(s) => {
                    bound = Some((p, s));
                    break;
                }
                Err(e) => eprintln!("[codex-pet] http {addr} 不可用：{e}"),
            }
        }
        let Some((port, server)) = bound else {
            emit_bubble(&handle, "本地端口全被占用，外部 agent 连不上桌宠".into(), true);
            return;
        };
        write_endpoint(port);
        eprintln!("[codex-pet] listening 127.0.0.1:{port}");
        for mut req in server.incoming_requests() {
            let method = req.method().to_string();
            let url = req.url().to_string();
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);

            let (code, payload) = match (method.as_str(), url.as_str()) {
                ("POST", "/event") => {
                    let v = json_body(&body);
                    // gesture 是单次动作，不改变当前姿态；只有带 activity 时才切状态
                    if let Some(g) = v.get("gesture").and_then(Value::as_str).filter(|g| !g.is_empty()) {
                        let _ = handle.emit("pet:gesture", serde_json::json!({ "gesture": g }));
                    }
                    if let Some(a) = v.get("activity").and_then(Value::as_str) {
                        let activity = parse_activity(a);
                        let detail = v.get("detail").and_then(Value::as_str).unwrap_or("external").to_string();
                        emit_state(&handle, &machine, activity, &detail);
                    }
                    (200, "{\"ok\":true}".to_string())
                }
                ("POST", "/say") => {
                    let v = json_body(&body);
                    let text = v.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                    emit_bubble(&handle, text, true);
                    (200, "{\"ok\":true}".to_string())
                }
                ("POST", "/speak") => {
                    // 只出声不起气泡（气泡由 /say 负责）
                    let v = json_body(&body);
                    let text = v.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                    if text.trim().is_empty() {
                        (400, "{\"error\":\"text required\"}".to_string())
                    } else {
                        speak(&handle, text);
                        (200, "{\"ok\":true}".to_string())
                    }
                }
                ("POST", "/chat") => {
                    let v = json_body(&body);
                    let text = v.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
                    if text.is_empty() {
                        (400, "{\"error\":\"text required\"}".to_string())
                    } else {
                        let h = handle.clone();
                        let m = machine.clone();
                        thread::spawn(move || run_chat(h, m, text));
                        (202, "{\"ok\":true}".to_string())
                    }
                }
                ("GET", "/state") => {
                    let m = machine.lock().unwrap();
                    let v = serde_json::json!({ "activity": m.activity, "detail": m.detail });
                    (200, v.to_string())
                }
                ("POST", "/settings") => {
                    // 让外部脚本/agent 也能唤起设置窗口
                    match pet_open_settings(handle.clone()) {
                        Ok(()) => (200, "{\"ok\":true}".to_string()),
                        Err(e) => (500, format!("{{\"error\":\"{e}\"}}")),
                    }
                }
                _ => (404, "{}".to_string()),
            };
            let _ = req.respond(tiny_http::Response::from_string(payload).with_status_code(code));
        }
    });
}

/// 内置 Codex 适配器：tail 最新 rollout，把事件映射成状态。
fn configure_tail(app: &tauri::AppHandle, machine: Shared) {
    let handle = app.clone();
    thread::spawn(move || {
        let root = sessions_root();
        eprintln!("[codex-pet] watching {}", root.display());
        let mut tailer = Tailer::new();
        loop {
            for ev in tailer.poll(&root) {
                let changed = {
                    let mut m = machine.lock().unwrap();
                    m.on_event(&ev)
                };
                if let Some(state) = changed {
                    eprintln!("[codex-pet] -> {:?} ({})", state.activity, state.detail);
                    let _ = handle.emit("pet:state", state);
                }
            }
            let changed = {
                let mut m = machine.lock().unwrap();
                m.tick()
            };
            if let Some(state) = changed {
                eprintln!("[codex-pet] -> {:?} ({})", state.activity, state.detail);
                let _ = handle.emit("pet:state", state);
            }
            thread::sleep(POLL);
        }
    });
}

fn place_window(app: &tauri::AppHandle) {
    // 开机贴到主屏右下角，避免和别的窗口叠在一起
    if let Some(win) = app.get_webview_window("pet") {
        if let Ok(Some(monitor)) = win.primary_monitor() {
            let scale = monitor.scale_factor();
            let size = monitor.size();
            let origin = monitor.position();
            let win_size = win.outer_size().unwrap_or(tauri::PhysicalSize { width: 320, height: 380 });
            let margin = (36.0 * scale) as i32;
            let x = origin.x + size.width as i32 - win_size.width as i32 - margin;
            let y = origin.y + size.height as i32 - win_size.height as i32 - margin;
            let _ = win.set_position(tauri::PhysicalPosition { x, y });
        }
    }
}

// 托盘菜单或全局热键唤回：显示窗口、抢焦点，并通知前端撤掉 CSS 隐藏
fn show_pet(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("pet") {
        let _ = win.show();
        let _ = win.set_focus();
    }
    let _ = app.emit("pet:show", ());
}

fn setup_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示 alice", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    let mut tray = TrayIconBuilder::with_id("pet-tray")
        .tooltip("alice 还在这儿（Ctrl+Alt+P 唤回）")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_pet(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_pet(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn main() {
    // `--mcp`：由 agent 拉起的 MCP server，只走 stdio，不开窗口
    if std::env::args().any(|a| a == "--mcp") {
        std::process::exit(mcp::serve());
    }
    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        show_pet(app);
                    }
                })
                .build(),
        )
        .setup(|app| {
            let machine: Shared = Arc::new(Mutex::new(Machine::new()));
            app.manage(machine.clone());
            let handle = app.handle().clone();
            // 语音开关、缩放、置顶都跟着配置走
            let cfg = load_config();
            VOICE.store(cfg.voice.unwrap_or(true), Ordering::Relaxed);
            // 先按配置把窗口尺寸调好，再贴到右下角，位置才准
            apply_scale(&handle, cfg.scale.unwrap_or(1.0));
            if !cfg.always_on_top.unwrap_or(true) {
                if let Some(win) = handle.get_webview_window("pet") {
                    let _ = win.set_always_on_top(false);
                }
            }
            place_window(&handle);
            setup_tray(&handle)?;
            // 热键被别人占了也不算致命，托盘和圆点仍能唤回
            if let Err(e) = app.global_shortcut().register(HOTKEY) {
                eprintln!("[codex-pet] 注册热键 {HOTKEY} 失败：{e}");
            }
            configure_http(&handle, machine.clone());
            configure_tail(&handle, machine);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            pet_chat,
            pet_ui_state,
            pet_set_voice,
            pet_get_config,
            pet_save_config,
            pet_tts_providers,
            pet_preview_voice,
            pet_open_settings,
            pet_set_topmost,
            pet_set_scale,
            pet_voice_start,
            pet_voice_stop,
            pet_voice_cancel,
            pet_hide_window,
            pet_quit
        ])
        .run(tauri::generate_context!())
        .expect("error while running codex-pet");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// 起一个最小 HTTP 服务，把收到的请求原文记下来，然后回一个固定响应。
    /// `body` 是响应体，Content-Length 按实际字节数算（中文是 UTF-8，三个字节）。
    fn http_once(body: &str) -> (u16, Arc<Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let captured = Arc::new(Mutex::new(String::new()));
        let sink = captured.clone();
        let body = body.to_string();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else { return };
            let mut got = String::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = match sock.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                got.push_str(&String::from_utf8_lossy(&chunk[..n]));
                // 头里拿到 Content-Length 就知道 body 到哪了
                if let Some(idx) = got.find("\r\n\r\n") {
                    let head = &got[..idx];
                    let len = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            if k.trim().eq_ignore_ascii_case("content-length") {
                                v.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    if got.len() >= idx + 4 + len {
                        break;
                    }
                }
            }
            *sink.lock().unwrap() = got;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.as_bytes().len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
            let _ = sock.shutdown(std::net::Shutdown::Both);
        });
        (port, captured)
    }

    fn fake_wav() -> PathBuf {
        // 文件名带上测试名：测试是并行跑的，共用一个文件会互相删掉
        let name = std::thread::current()
            .name()
            .unwrap_or("test")
            .replace("::", "_");
        let p = std::env::temp_dir().join(format!("codex-pet-test-{name}.wav"));
        fs::write(&p, b"RIFF----WAVEfmt ").expect("写测试 wav");
        p
    }

    #[test]
    fn openai模式的地址优先用api_endpoint() {
        let asr = AsrConfig {
            provider: Some("openai".into()),
            endpoint: Some("http://127.0.0.1:8080/inference".into()),
            api_endpoint: Some("https://example.com/v1/audio/transcriptions".into()),
            ..Default::default()
        };
        // endpoint 是本机 whisper 的地址，不能被当成云端转写地址
        assert_eq!(
            openai_endpoint(&asr),
            "https://example.com/v1/audio/transcriptions"
        );
    }

    #[test]
    fn openai模式没填地址时用官方地址而不是whisper的() {
        let asr = AsrConfig {
            provider: Some("openai".into()),
            endpoint: Some("http://127.0.0.1:8080/inference".into()),
            api_endpoint: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(
            openai_endpoint(&asr),
            "https://api.openai.com/v1/audio/transcriptions"
        );
    }

    #[test]
    fn openai模式能上传音频并解析结果() {
        let (port, captured) = http_once("{\"text\":\"你好呀\"}");
        let wav = fake_wav();
        let asr = AsrConfig {
            provider: Some("openai".into()),
            api_endpoint: Some(format!("http://127.0.0.1:{port}/v1/audio/transcriptions")),
            api_key: Some("test-key-abc".into()),
            model: Some("whisper-1".into()),
            language: Some("zh".into()),
            ..Default::default()
        };
        let text = asr_openai(&wav, &asr).expect("识别应该成功");
        assert_eq!(text, "你好呀");
        let req = captured.lock().unwrap().clone();
        // key 走 stdin 传进 curl，但请求头里必须真的带上
        assert!(req.contains("Authorization: Bearer test-key-abc"), "实际请求：{req}");
        // 音频是 multipart 上传
        assert!(req.contains("name=\"file\""), "实际请求：{req}");
        assert!(req.contains("name=\"model\"") && req.contains("whisper-1"));
        let _ = fs::remove_file(&wav);
    }

    #[test]
    fn openai模式没key时给出明确提示() {
        let wav = fake_wav();
        let asr = AsrConfig {
            provider: Some("openai".into()),
            api_endpoint: Some("http://127.0.0.1:1/v1/audio/transcriptions".into()),
            api_key: Some(String::new()),
            api_key_env: Some("PET_TEST_NOT_SET_KEY".into()),
            ..Default::default()
        };
        let err = asr_openai(&wav, &asr).unwrap_err();
        assert!(err.contains("PET_TEST_NOT_SET_KEY"), "实际错误：{err}");
        let _ = fs::remove_file(&wav);
    }

    #[test]
    fn 外部命令模式能拿到标准输出() {
        let wav = fake_wav();
        let asr = AsrConfig {
            provider: Some("command".into()),
            command: Some("echo 识别出来的文字".into()),
            language: Some("zh".into()),
            ..Default::default()
        };
        let text = asr_command(&wav, &asr).expect("命令应该跑通");
        assert!(text.contains("识别出来的文字"), "实际输出：{text}");
        let _ = fs::remove_file(&wav);
    }

    #[test]
    fn 外部命令模式会把占位符换成真实路径() {
        let wav = fake_wav();
        let asr = AsrConfig {
            provider: Some("command".into()),
            command: Some("echo {audio_file} {language}".into()),
            language: Some("ja".into()),
            ..Default::default()
        };
        let text = asr_command(&wav, &asr).expect("命令应该跑通");
        assert!(text.contains(&wav.to_string_lossy().to_string()), "实际输出：{text}");
        assert!(text.contains("ja"), "实际输出：{text}");
        let _ = fs::remove_file(&wav);
    }

    #[test]
    fn 外部命令没填时给出明确提示() {
        let wav = fake_wav();
        let asr = AsrConfig {
            provider: Some("command".into()),
            command: Some(String::new()),
            ..Default::default()
        };
        let err = asr_command(&wav, &asr).unwrap_err();
        assert!(err.contains("外部命令"), "实际错误：{err}");
        let _ = fs::remove_file(&wav);
    }

    #[test]
    fn 幻听黑名单会被过滤掉() {
        for p in ["谢谢观看", "谢 谢 观 看", "thank you for watching", "字幕由amara.org社区提供"] {
            assert!(is_asr_hallucination(p), "应该被过滤：{p}");
        }
        assert!(!is_asr_hallucination("帮我看看这个文件"));
    }

    #[test]
    fn key留空时沿用磁盘上保存的() {
        let old = serde_json::json!({ "tts": { "api_key": "old-key" } });
        let mut next = serde_json::json!({ "tts": { "api_key": "", "model": "x" } });
        merge_secret(&mut next, Some(&old), "tts");
        assert_eq!(next["tts"]["api_key"], "old-key");
        assert_eq!(next["tts"]["model"], "x");
    }

    #[test]
    fn key填了新值就以新值为准() {
        let old = serde_json::json!({ "tts": { "api_key": "old-key" } });
        let mut next = serde_json::json!({ "tts": { "api_key": "new-key" } });
        merge_secret(&mut next, Some(&old), "tts");
        assert_eq!(next["tts"]["api_key"], "new-key");
    }

    #[test]
    fn 磁盘上没有key时不硬塞() {
        let old = serde_json::json!({ "tts": { "model": "x" } });
        let mut next = serde_json::json!({ "tts": { "api_key": "" } });
        merge_secret(&mut next, Some(&old), "tts");
        assert_eq!(next["tts"]["api_key"], "");
    }
}
