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

/// 语音输入：ffmpeg 录麦克风，交给本机 whisper-server 转文字。
#[derive(Deserialize, Default)]
struct AsrConfig {
    /// whisper.cpp server 的转写地址，默认 http://127.0.0.1:8080/inference
    #[serde(default)]
    endpoint: Option<String>,
    /// ffmpeg 路径，留空自动找（exe 同目录 / 上层 tools 目录 / PATH）
    #[serde(default)]
    ffmpeg: Option<String>,
    /// 识别语言，默认 zh
    #[serde(default)]
    language: Option<String>,
    /// dshow 录音设备名，留空自动挑第一个麦克风
    #[serde(default)]
    device: Option<String>,
}

/// 语音合成配置。默认走云端 MiniMax（中文可用），失败回退本地 SAPI。
#[derive(Deserialize, Default)]
struct TtsConfig {
    /// `minimax`（默认）或 `sapi`
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    voice_id: Option<String>,
    /// API key 兜底。优先读环境变量，这里只是环境变量拿不到时的备份
    #[serde(default)]
    api_key: Option<String>,
    /// key 所在的环境变量名，默认 MINIMAX_API_KEY
    #[serde(default)]
    api_key_env: Option<String>,
    /// 回退到 SAPI 时指定语音名，留空则自动挑中文
    #[serde(default)]
    sapi_voice: Option<String>,
    /// 走云端 TTS 时的 HTTP 代理，例如 http://127.0.0.1:7890；留空沿用系统代理
    #[serde(default)]
    proxy: Option<String>,
}

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
            "engine": "minimax",
            "endpoint": "https://api.minimaxi.com",
            "model": "speech-2.8-turbo",
            "voice_id": "female-tianmei",
            "api_key_env": "MINIMAX_API_KEY",
            "sapi_voice": "",
            "proxy": ""
        },
        "asr": {
            "endpoint": "http://127.0.0.1:8080/inference",
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

/// TTS 辅助脚本。走 `-File` 而不是 `-Command`：PowerShell 5.1 会把长 `-Command`
/// 字符串截断，导致脚本拿不到参数（SAPI 那条路会报 0x20 音频设备错误）。
///
/// 主路径是 MiniMax 云端合成（系统没装中文语音包也能念中文），失败才回退本地 SAPI。
/// key 通过环境变量 `PET_TTS_KEY` 传入，不出现在命令行里。
const TTS_SCRIPT: &str = r#"param(
  [Parameter(Mandatory=$true)][string]$TextFile,
  [string]$Engine = "minimax",
  [string]$Endpoint = "https://api.minimaxi.com",
  [string]$Model = "speech-2.8-turbo",
  [string]$VoiceId = "female-shaonv",
  [int]$TimeoutSec = 40,
  [string]$SapiVoice = "",
  [string]$Proxy = ""
)
$ErrorActionPreference = "Continue"
# 必须用 .NET 读取：Get-Content -Raw 返回的字符串带 PS 扩展属性（PSPath 等），
# ConvertTo-Json 会把它当对象递归展开，请求体从几百字节膨胀到几十 MB，请求必然超时。
$text = [System.IO.File]::ReadAllText($TextFile, [System.Text.Encoding]::UTF8)
if (-not $text -or -not $text.Trim()) { exit 0 }

$errFile = Join-Path $env:TEMP "codex-pet-tts.err"

function Play-Wav([string]$path) {
  $p = New-Object System.Media.SoundPlayer $path
  $p.PlaySync()
  $p.Dispose()
}

function Get-HexBytes([string]$hex) {
  try {
    return ([System.Runtime.Remoting.Metadata.W3cXsd2001.SoapHexBinary]::Parse($hex)).Value
  } catch {
    $raw = New-Object byte[] ([int]($hex.Length / 2))
    for ($i = 0; $i -lt $raw.Length; $i++) {
      $raw[$i] = [Convert]::ToByte($hex.Substring($i * 2, 2), 16)
    }
    return $raw
  }
}

if ($Engine -eq "minimax" -and $env:PET_TTS_KEY) {
  $payload = @{
    model = $Model
    text = $text
    stream = $false
    language_boost = "Chinese"
    voice_setting = @{ voice_id = $VoiceId; speed = 1; vol = 1; pitch = 0; emotion = "happy" }
    audio_setting = @{ sample_rate = 32000; bitrate = 128000; format = "wav"; channel = 1 }
  } | ConvertTo-Json -Depth 6 -Compress
  $body = [System.Text.Encoding]::UTF8.GetBytes($payload)
  [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

  # 云端偶发慢/断连（实测 2-5 秒成功，偶尔整请求挂住），重试几次再退回 SAPI
  $hex = $null
  $req = @{
    Uri = $Endpoint + "/v1/t2a_v2"
    Method = "Post"
    Headers = @{ Authorization = "Bearer " + $env:PET_TTS_KEY }
    ContentType = "application/json; charset=utf-8"
    Body = $body
    TimeoutSec = $TimeoutSec
  }
  # 显式代理：PS 5.1 走系统代理时，部分 Clash 配置下会整请求挂到超时
  if ($Proxy) { $req["Proxy"] = $Proxy; $req["ProxyUseDefaultCredentials"] = $false }
  for ($attempt = 1; $attempt -le 3; $attempt++) {
    try {
      $resp = Invoke-RestMethod @req
      if ($resp.base_resp.status_code -ne 0) { throw ("api " + $resp.base_resp.status_msg) }
      if (-not $resp.data.audio) { throw "empty audio" }
      $hex = $resp.data.audio
      break
    } catch {
      try { [System.IO.File]::WriteAllText($errFile, ("try " + $attempt + ": " + $_.Exception.Message)) } catch { }
      Start-Sleep -Seconds 1
    }
  }

  if ($hex) {
    try {
      $wav = Join-Path $env:TEMP ("pet-tts-" + [guid]::NewGuid().ToString("N") + ".wav")
      [System.IO.File]::WriteAllBytes($wav, (Get-HexBytes $hex))
      try { Play-Wav $wav } finally { Remove-Item $wav -ErrorAction SilentlyContinue }
      Remove-Item $errFile -ErrorAction SilentlyContinue
      exit 0
    } catch {
      try { [System.IO.File]::WriteAllText($errFile, ("play: " + $_.Exception.Message)) } catch { }
    }
  }
}

Add-Type -AssemblyName System.Speech
$s = New-Object System.Speech.Synthesis.SpeechSynthesizer
$picked = ""
try {
  if ($SapiVoice) {
    $s.SelectVoice($SapiVoice)
    $picked = $SapiVoice
  } else {
    # 已经指定了女声，就别让系统默认的男声（如 Microsoft David）顶上来
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

fn tts_script_path() -> PathBuf {
    std::env::temp_dir().join("codex-pet-tts.ps1")
}

fn ensure_tts_script() -> Option<PathBuf> {
    let path = tts_script_path();
    let current = fs::read_to_string(&path).unwrap_or_default();
    // 必须带 BOM 写入：PowerShell 5.1 对无 BOM 的脚本按 ANSI 解码，中文注释会吞掉
    // 后面的行（实测把下一行的赋值整行变成注释，脚本静默 exit 0，一点声音都没有）
    if current.trim_start_matches('\u{feff}') != TTS_SCRIPT {
        fs::write(&path, format!("\u{feff}{TTS_SCRIPT}")).ok()?;
    }
    Some(path)
}

/// 读用户级环境变量（HKCU\Environment）。
/// 桌宠若在 setx 之前就启动了，进程里拿不到新 key，这里兜底一次。
#[cfg(windows)]
fn user_env_var(name: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = Command::new("reg")
        .args(["query", "HKCU\\Environment", "/v", name])
        .stdin(Stdio::null())
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // 形如：    MINIMAX_API_KEY    REG_SZ    sk-cp-...
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.contains("REG_SZ"))
        .and_then(|l| l.split_whitespace().last())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(not(windows))]
fn user_env_var(_name: &str) -> Option<String> {
    None
}

/// 用 Windows 内置 SAPI 朗读。文本走临时文件，避开 PowerShell 的编码与引号问题。
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

    let Some(script) = ensure_tts_script() else {
        eprintln!("[codex-pet] 写 TTS 脚本失败");
        return;
    };

    let seq = TTS_SEQ.fetch_add(1, Ordering::Relaxed);
    let text_file = std::env::temp_dir().join(format!("codex-pet-tts-{}-{seq}.txt", std::process::id()));
    if fs::write(&text_file, spoken.as_bytes()).is_err() {
        return;
    }

    let cfg = load_config();
    let tts = cfg.tts.unwrap_or_default();
    let engine = tts.engine.unwrap_or_else(|| "minimax".to_string());
    // key 优先环境变量（默认 MINIMAX_API_KEY），拿不到再用 pet.json 里的兜底
    let key_env = tts.api_key_env.unwrap_or_else(|| "MINIMAX_API_KEY".to_string());
    let key = std::env::var(&key_env)
        .ok()
        .filter(|k| !k.trim().is_empty())
        // 进程环境拿不到时读用户环境变量（setx 后没重启桌宠的情况）
        .or_else(|| user_env_var(&key_env))
        .or_else(|| tts.api_key.clone())
        .unwrap_or_default();

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
    .arg(&text_file)
    .arg("-Engine")
    .arg(&engine)
    .arg("-Endpoint")
    .arg(tts.endpoint.unwrap_or_else(|| "https://api.minimaxi.com".to_string()))
    .arg("-Model")
    .arg(tts.model.unwrap_or_else(|| "speech-2.8-turbo".to_string()))
    .arg("-VoiceId")
    .arg(tts.voice_id.unwrap_or_else(|| "female-tianmei".to_string()));
    if let Some(v) = tts.sapi_voice {
        if !v.trim().is_empty() {
            cmd.arg("-SapiVoice").arg(v);
        }
    }
    // 显式代理：pet.json 的 tts.proxy 优先，其次是本进程的 HTTPS_PROXY/HTTP_PROXY
    let proxy = tts
        .proxy
        .clone()
        .filter(|p| !p.trim().is_empty())
        .or_else(|| std::env::var("HTTPS_PROXY").ok())
        .or_else(|| std::env::var("HTTP_PROXY").ok());
    if let Some(p) = proxy {
        cmd.arg("-Proxy").arg(p);
    }
    // key 走环境变量，不进命令行（避免出现在进程列表里）
    cmd.env("PET_TTS_KEY", key);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    if let Ok(mut child) = cmd.spawn() {
        // 朗读是串行的事，别堵住调用方线程
        let handle = app.clone();
        thread::spawn(move || {
            let _ = handle.emit("pet:speaking", serde_json::json!({ "on": true }));
            let _ = child.wait();
            let _ = handle.emit("pet:speaking", serde_json::json!({ "on": false }));
            let _ = fs::remove_file(&text_file);
        });
    } else {
        let _ = fs::remove_file(&text_file);
    }
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
    // 界面不回显密钥，保存时把磁盘上原有的 tts.api_key 保回来
    let old: Option<Value> = fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok());
    let old_key = old
        .as_ref()
        .and_then(|o| o.get("tts"))
        .and_then(|t| t.get("api_key"))
        .cloned();
    if let (Some(key), Some(tts)) = (old_key, next.get_mut("tts").and_then(Value::as_object_mut)) {
        if !tts.contains_key("api_key") {
            tts.insert("api_key".to_string(), key);
        }
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

#[tauri::command]
fn pet_preview_voice(app: tauri::AppHandle, text: Option<String>) {
    let t = text
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "你好呀，我是 alice，很高兴见到你。".to_string());
    speak(&app, t);
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
    const PHRASES: [&str; 9] = [
        "谢谢大家收看",
        "謝謝大家收看",
        "谢谢大家",
        "謝謝大家",
        "谢谢观看",
        "謝謝觀看",
        "请不吝点赞订阅",
        "thankyou",
        "thankyouforwatching",
    ];
    PHRASES.iter().any(|p| norm == *p)
}

fn transcribe(wav: &Path, cfg: &Config) -> Result<String, String> {
    let asr = cfg.asr.as_ref();
    let endpoint = asr
        .and_then(|a| a.endpoint.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8080/inference".to_string());
    let lang = asr
        .and_then(|a| a.language.clone())
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
        &endpoint,
    ])
    .stdin(Stdio::null());
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
    let text = clean_asr(v.get("text").and_then(Value::as_str).unwrap_or(""));
    Ok(if is_asr_hallucination(&text) { String::new() } else { text })
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
