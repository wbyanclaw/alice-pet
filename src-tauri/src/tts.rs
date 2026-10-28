//! 云端 TTS：供应商可切换，不写死在代码里。
//!
//! 设计取舍：
//! - 请求统一用 `curl.exe`（Windows 10+ 自带，ASR 那条路已经在用），不引 HTTP 客户端依赖；
//! - 不同厂商的差异只体现在「预设」里：请求体怎么拼、鉴权头叫什么、
//!   音频在响应的哪个字段、是 hex / base64 / 二进制；
//! - 播放和本机 SAPI 仍走 PowerShell——Windows 专有，Rust 侧重写成本高；
//! - key 不进命令行也不落盘：通过 stdin 把 curl 配置喂给 `curl -K -`。

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 一家 TTS 供应商的预设。用户选预设后这些值自动填进设置表单，
/// 想改哪个改哪个，不改就能直接用。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub id: String,
    pub label: String,
    /// 接口基址
    pub endpoint: String,
    /// 请求路径，拼在 endpoint 后面
    pub path: String,
    pub model: String,
    pub voice: String,
    /// key 所在的环境变量名
    pub api_key_env: String,
    /// 响应里音频字段的点路径，例如 "data.audio"
    pub audio_path: String,
    /// hex | base64 | url | binary
    pub audio_format: String,
    /// 鉴权请求头名与前缀
    pub auth_header: String,
    pub auth_prefix: String,
    /// 额外请求头
    pub headers: BTreeMap<String, String>,
    /// 是否需要 key
    pub needs_key: bool,
    /// 是否是本机引擎（不需要接口/key）
    pub local: bool,
    /// 接口地址输入框的占位提示
    pub endpoint_hint: String,
    /// 给用户看的一句话说明
    pub hint: String,
}

/// 供应商预设表。加一家新厂商只需在这里加一条，不用改其它代码。
fn preset(
    id: &str,
    label: &str,
    endpoint: &str,
    path: &str,
    model: &str,
    voice: &str,
    api_key_env: &str,
    audio_path: &str,
    audio_format: &str,
    auth_header: &str,
    auth_prefix: &str,
    needs_key: bool,
    local: bool,
    endpoint_hint: &str,
    hint: &str,
) -> ProviderInfo {
    ProviderInfo {
        id: id.into(),
        label: label.into(),
        endpoint: endpoint.into(),
        path: path.into(),
        model: model.into(),
        voice: voice.into(),
        api_key_env: api_key_env.into(),
        audio_path: audio_path.into(),
        audio_format: audio_format.into(),
        auth_header: auth_header.into(),
        auth_prefix: auth_prefix.into(),
        headers: BTreeMap::new(),
        needs_key,
        local,
        endpoint_hint: endpoint_hint.into(),
        hint: hint.into(),
    }
}

/// 全部预设。设置界面拿这个列表渲染下拉框。
pub fn provider_list() -> Vec<ProviderInfo> {
    vec![
        preset(
            "minimax",
            "MiniMax（中文最自然）",
            "https://api.minimaxi.com",
            "/v1/t2a_v2",
            "speech-2.8-turbo",
            "female-tianmei",
            "MINIMAX_API_KEY",
            "data.audio",
            "hex",
            "Authorization",
            "Bearer ",
            true,
            false,
            "https://api.minimaxi.com",
            "国际站域名；音频是 hex 编码，需解码后播放",
        ),
        preset(
            "openai",
            "OpenAI 兼容（含 Groq / 硅基流动 / 火山方舟等）",
            "https://api.openai.com/v1",
            "/audio/speech",
            "gpt-4o-mini-tts",
            "nova",
            "OPENAI_API_KEY",
            "",
            "binary",
            "Authorization",
            "Bearer ",
            true,
            false,
            "https://api.openai.com/v1",
            "任何 OpenAI 兼容 /v1/audio/speech 的服务都能用，换接口地址即可",
        ),
        preset(
            "azure",
            "Azure OpenAI",
            "",
            "/openai/deployments/{model}/audio/speech?api-version=2024-10-21",
            "你的部署名",
            "zh-CN-XiaoxiaoNeural",
            "AZURE_OPENAI_API_KEY",
            "",
            "binary",
            "api-key",
            "",
            true,
            false,
            "https://你的资源名.openai.azure.com",
            "鉴权头是 api-key 而不是 Authorization；路径里的 {model} 会替换成部署名",
        ),
        preset(
            "custom",
            "自定义（自己写请求模板）",
            "",
            "/v1/tts",
            "your-model",
            "your-voice",
            "CUSTOM_TTS_API_KEY",
            "data.audio",
            "hex",
            "Authorization",
            "Bearer ",
            true,
            false,
            "https://your-api.example.com",
            "请求体模板里用 {{text}} {{model}} {{voice}} {{emotion}} {{speed}} 占位；写成 \"{{text}}\" 或 {{text}} 都行",
        ),
        preset(
            "sapi",
            "本机 SAPI（离线，无需 key）",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            false,
            true,
            "",
            "用 Windows 自带语音；本机没装中文语音包时读中文不出声",
        ),
        preset(
            "command",
            "外部命令（edge-tts / piper 等）",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            false,
            true,
            "",
            "命令里可用 {text_file} {out_file} {voice} {model} 占位",
        ),
    ]
}

pub fn provider(id: &str) -> Option<ProviderInfo> {
    provider_list().into_iter().find(|p| p.id == id)
}

/// TTS 配置。字段全是 Option——没写就用预设的默认值，
/// 这样加新字段不会让老配置文件失效。
#[derive(Deserialize, Default, Clone, Debug)]
pub struct TtsSettings {
    /// 供应商 id，见 provider_list()
    pub provider: Option<String>,
    pub endpoint: Option<String>,
    pub path: Option<String>,
    pub model: Option<String>,
    /// 音色。voice_id 是旧字段名，保留兼容。
    pub voice: Option<String>,
    pub voice_id: Option<String>,
    /// 直接填在配置里的 key（优先级最高）
    pub api_key: Option<String>,
    /// key 所在环境变量（优先级次之）
    pub api_key_env: Option<String>,
    pub proxy: Option<String>,
    /// 额外请求头
    pub headers: Option<BTreeMap<String, String>>,
    /// 自定义请求体模板
    pub body_template: Option<String>,
    /// 响应里音频字段的点路径
    pub audio_path: Option<String>,
    /// hex | base64 | url | binary
    pub audio_format: Option<String>,
    pub speed: Option<f64>,
    pub emotion: Option<String>,
    /// 本机 SAPI 音色
    pub sapi_voice: Option<String>,
    /// 外部命令
    pub command: Option<String>,
    #[serde(default)]
    pub timeout_sec: Option<u64>,
}

/// 解析后的生效配置：预设 + 用户覆盖。
struct Resolved {
    info: ProviderInfo,
    endpoint: String,
    path: String,
    model: String,
    voice: String,
    api_key: String,
    api_key_env: String,
    proxy: String,
    headers: BTreeMap<String, String>,
    body_template: String,
    audio_path: String,
    audio_format: String,
    speed: f64,
    emotion: String,
    command: String,
    timeout_sec: u64,
}

fn pick_str(user: &Option<String>, preset_value: &str) -> String {
    match user {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => preset_value.to_string(),
    }
}

impl TtsSettings {
    /// 把预设和用户配置合成一份生效配置。
    fn resolve(&self) -> Resolved {
        let pid = self
            .provider
            .as_deref()
            .unwrap_or("minimax")
            .to_string();
        let info = provider(&pid).unwrap_or_else(|| provider("minimax").unwrap());
        let voice_pick = self
            .voice
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.voice_id.clone())
            .unwrap_or_default();
        Resolved {
            endpoint: pick_str(&self.endpoint, &info.endpoint),
            path: pick_str(&self.path, &info.path),
            model: pick_str(&self.model, &info.model),
            voice: pick_str(&Some(voice_pick), &info.voice),
            api_key: self.api_key.clone().unwrap_or_default().trim().to_string(),
            api_key_env: pick_str(&self.api_key_env, &info.api_key_env),
            proxy: self.proxy.clone().unwrap_or_default().trim().to_string(),
            headers: self.headers.clone().unwrap_or_default(),
            body_template: self.body_template.clone().unwrap_or_default(),
            audio_path: pick_str(&self.audio_path, &info.audio_path),
            audio_format: pick_str(&self.audio_format, &info.audio_format),
            speed: self.speed.unwrap_or(1.0),
            emotion: pick_str(&self.emotion, "happy"),
            command: self.command.clone().unwrap_or_default().trim().to_string(),
            timeout_sec: self.timeout_sec.unwrap_or(40),
            info,
        }
    }

    /// 实际生效的 provider id，界面上显示用。
    pub fn provider_id(&self) -> String {
        self.provider.clone().unwrap_or_else(|| "minimax".into())
    }
}

/// 合成结果：音频字节 + 格式。
#[derive(Debug)]
pub struct Audio {
    pub bytes: Vec<u8>,
    /// wav / mp3 / ogg ...
    pub format: String,
}

/// 合成一段语音。返回 Err 时带上人话原因，界面上直接显示。
pub fn synthesize(cfg: &TtsSettings, text: &str) -> Result<Audio, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("要念的内容是空的".into());
    }
    let r = cfg.resolve();
    match r.info.id.as_str() {
        "command" => run_command(&r, text),
        _ => call_cloud(&r, text),
    }
}

// ---------- 外部命令 ----------

fn run_command(r: &Resolved, text: &str) -> Result<Audio, String> {
    if r.command.is_empty() {
        return Err("外部命令模式还没填命令".into());
    }
    let seq = std::process::id();
    let text_file = std::env::temp_dir().join(format!("codex-pet-tts-{seq}.txt"));
    // 输出文件用 .wav 后缀：ffmpeg 这类工具靠扩展名判断输出格式，
    // 实际是什么格式我们靠文件头猜，不影响识别。
    let out_file = std::env::temp_dir().join(format!("codex-pet-tts-{seq}.wav"));
    fs::write(&text_file, text.as_bytes()).map_err(|e| format!("写临时文件失败：{e}"))?;
    let _ = fs::remove_file(&out_file);

    let expanded = r
        .command
        .replace("{text_file}", &text_file.to_string_lossy())
        .replace("{out_file}", &out_file.to_string_lossy())
        .replace("{voice}", &r.voice)
        .replace("{model}", &r.model);

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
    let _ = fs::remove_file(&text_file);

    if !out.status.success() {
        let err = crate::decode_console_output(&out.stderr);
        return Err(format!(
            "外部命令失败（{}）：{}",
            out.status,
            err.lines().next().unwrap_or("").trim()
        ));
    }
    let bytes = fs::read(&out_file).map_err(|e| format!("外部命令没写出音频文件：{e}"))?;
    let _ = fs::remove_file(&out_file);
    if bytes.is_empty() {
        return Err("外部命令产出的是空文件".into());
    }
    let format = guess_format(&bytes).to_string();
    Ok(Audio { bytes, format })
}

// ---------- 云端 ----------

fn call_cloud(r: &Resolved, text: &str) -> Result<Audio, String> {
    let key = resolve_key(r);
    if r.info.needs_key && key.is_empty() {
        return Err(format!(
            "没拿到 API key：填在设置里，或设环境变量 {}",
            if r.api_key_env.is_empty() {
                "（未指定变量名）"
            } else {
                &r.api_key_env
            }
        ));
    }

    let body = build_body(r, text)?;
    if r.endpoint.is_empty() {
        return Err("接口地址还没填".into());
    }
    let url = format!(
        "{}{}",
        r.endpoint.trim_end_matches('/'),
        r.path.replace("{model}", &r.model)
    );

    let dir = std::env::temp_dir();
    let tag = format!("{}-{}", std::process::id(), r.info.id);
    let body_file = dir.join(format!("codex-pet-tts-{tag}.req.json"));
    let resp_file = dir.join(format!("codex-pet-tts-{tag}.resp"));
    fs::write(&body_file, body.as_bytes()).map_err(|e| format!("写请求体失败：{e}"))?;

    // 鉴权头和自定义头通过 stdin 交给 curl（curl -K - 从 stdin 读配置）。
    // 这样 key 既不在进程命令行里（ps 能看到），也不会落盘成明文文件。
    let mut curl_cfg = String::new();
    if r.info.needs_key {
        curl_cfg.push_str(&format!(
            "header = {}\n",
            curl_quote(&format!("{}: {}{}", r.info.auth_header, r.info.auth_prefix, key))
        ));
    }
    for (k, v) in &r.headers {
        curl_cfg.push_str(&format!("header = {}\n", curl_quote(&format!("{k}: {v}"))));
    }

    let mut cmd = Command::new("curl");
    cmd.arg("-sS")
        .arg("--fail-with-body")
        .arg("-X")
        .arg("POST")
        .arg("--max-time")
        .arg(r.timeout_sec.to_string())
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("--data-binary")
        .arg(format!("@{}", body_file.to_string_lossy()))
        .arg("-o")
        .arg(&resp_file)
        .arg("-K")
        .arg("-")
        .arg(&url);
    if !r.proxy.is_empty() {
        cmd.arg("-x").arg(&r.proxy);
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
    let out = child.wait_with_output().map_err(|e| format!("等 curl 结束失败：{e}"))?;
    let resp = fs::read(&resp_file).unwrap_or_default();
    let _ = fs::remove_file(&body_file);
    let _ = fs::remove_file(&resp_file);

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let body_msg = String::from_utf8_lossy(&resp);
        let detail = if body_msg.trim().is_empty() {
            err.lines().next().unwrap_or("").trim().to_string()
        } else {
            body_msg.chars().take(300).collect()
        };
        return Err(format!("云端返回错误：{detail}"));
    }

    decode_audio(r, &resp)
}

/// curl 配置里的字符串要转义：双引号、反斜杠、换行都有特殊含义。
fn curl_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 按预设拼请求体。
fn build_body(r: &Resolved, text: &str) -> Result<String, String> {
    // 自定义模板优先——用户可能完全不管预设的字段。
    if !r.body_template.trim().is_empty() {
        return Ok(apply_template(
            &r.body_template,
            text,
            &r.model,
            &r.voice,
            &r.emotion,
            r.speed,
        ));
    }
    let v = match r.info.id.as_str() {
        "minimax" => json!({
            "model": r.model,
            "text": text,
            "stream": false,
            "language_boost": "Chinese",
            "voice_setting": {
                "voice_id": r.voice,
                "speed": r.speed,
                "vol": 1,
                "pitch": 0,
                "emotion": r.emotion,
            },
            "audio_setting": {
                "sample_rate": 32000,
                "bitrate": 128000,
                "format": "wav",
                "channel": 1,
            }
        }),
        "openai" => json!({
            "model": r.model,
            "input": text,
            "voice": r.voice,
            "response_format": "wav",
            "speed": r.speed,
        }),
        "azure" => json!({
            "input": text,
            "voice": r.voice,
            "response_format": "wav",
            "speed": r.speed,
        }),
        other => return Err(format!("未知的语音供应商：{other}")),
    };
    serde_json::to_string(&v).map_err(|e| format!("拼请求体失败：{e}"))
}

/// 模板占位符替换。占位符两种写法都支持：
///
/// - `"text": "{{text}}"` —— 占位符外面带引号，就只替换引号里的内容（做 JSON 转义）；
/// - `"text": {{text}}`   —— 不带引号，就替换成完整的 JSON 字符串字面量（自带引号）。
///
/// 用户按写普通 JSON 的直觉写就行，不用关心引号该加在哪边。
/// `{{speed}}` 是数字，始终输出裸数字。
fn apply_template(
    tpl: &str,
    text: &str,
    model: &str,
    voice: &str,
    emotion: &str,
    speed: f64,
) -> String {
    let mut out = String::with_capacity(tpl.len() + 32);
    let mut rest = tpl;
    loop {
        let Some(rel) = rest.find("{{") else {
            out.push_str(rest);
            break;
        };
        let (head, tail) = rest.split_at(rel);
        out.push_str(head);
        let tail = &tail[2..];
        let Some(end_rel) = tail.find("}}") else {
            // 没闭合的 {{ 当普通文本
            out.push_str("{{");
            out.push_str(tail);
            break;
        };
        let name = &tail[..end_rel];
        let after = &tail[end_rel + 2..];

        let text_value = match name {
            "text" => Some(text),
            "model" => Some(model),
            "voice" => Some(voice),
            "emotion" => Some(emotion),
            _ => None,
        };
        if name == "speed" {
            out.push_str(&format!("{speed:?}"));
        } else if let Some(v) = text_value {
            // 占位符外面已经带引号了，只补内容；否则补一整个 JSON 字符串。
            if out.ends_with('"') && after.starts_with('"') {
                out.push_str(&json_escape(v));
            } else {
                out.push_str(&json_str(v));
            }
        } else {
            // 不认识的占位符原样留着，方便用户看出是哪写错了
            out.push_str("{{");
            out.push_str(name);
            out.push_str("}}");
        }
        rest = after;
    }
    out
}

/// 完整的 JSON 字符串字面量，带外层引号。
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// JSON 转义后的内容，不带外层引号——用在模板里已经有引号的地方。
fn json_escape(s: &str) -> String {
    let quoted = json_str(s);
    if quoted.len() >= 2 {
        quoted[1..quoted.len() - 1].to_string()
    } else {
        String::new()
    }
}

/// 从响应里取出音频。
fn decode_audio(r: &Resolved, resp: &[u8]) -> Result<Audio, String> {
    match r.audio_format.as_str() {
        "binary" => {
            if resp.len() >= 4 && &resp[..4] == b"RIFF" {
                return Ok(Audio { bytes: resp.to_vec(), format: "wav".into() });
            }
            // 不是音频，多半是接口返回了 JSON 错误
            let msg = String::from_utf8_lossy(resp);
            if let Ok(v) = serde_json::from_str::<Value>(msg.trim()) {
                if let Some(e) = v.get("error") {
                    let d = e
                        .get("message")
                        .and_then(Value::as_str)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| e.to_string());
                    return Err(format!("云端返回错误：{d}"));
                }
            }
            Err("云端没返回音频数据".into())
        }
        "hex" => {
            let v: Value = serde_json::from_slice(resp)
                .map_err(|_| "云端返回的不是 JSON".to_string())?;
            let hex = json_path(&v, &r.audio_path)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("响应里找不到音频字段 {}", r.audio_path))?;
            if hex.trim().is_empty() {
                return Err("云端返回了空音频".into());
            }
            let bytes = from_hex(hex)?;
            if bytes.is_empty() {
                return Err("云端返回了空音频".into());
            }
            Ok(Audio { bytes, format: "wav".into() })
        }
        "base64" => {
            let v: Value = serde_json::from_slice(resp)
                .map_err(|_| "云端返回的不是 JSON".to_string())?;
            let b64 = json_path(&v, &r.audio_path)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("响应里找不到音频字段 {}", r.audio_path))?;
            if b64.trim().is_empty() {
                return Err("云端返回了空音频".into());
            }
            let bytes = from_base64(b64)?;
            if bytes.is_empty() {
                return Err("云端返回了空音频".into());
            }
            let format = guess_format(&bytes).to_string();
            Ok(Audio { bytes, format })
        }
        "url" => {
            let v: Value = serde_json::from_slice(resp)
                .map_err(|_| "云端返回的不是 JSON".to_string())?;
            let url = json_path(&v, &r.audio_path)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("响应里找不到音频链接 {}", r.audio_path))?;
            let bytes = download(url)?;
            let format = guess_format(&bytes).to_string();
            Ok(Audio { bytes, format })
        }
        other => Err(format!("未知的音频格式：{other}")),
    }
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new("curl");
    cmd.arg("-sS")
        .arg("--fail-with-body")
        .arg("--max-time")
        .arg("120")
        .arg("-L")
        .arg(url);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| format!("下载音频失败：{e}"))?;
    if !out.status.success() {
        return Err("下载音频失败".into());
    }
    if out.stdout.is_empty() {
        return Err("下载到的是空文件".into());
    }
    Ok(out.stdout)
}

/// 按点路径取 JSON 里的字段，支持数组下标。
fn json_path<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for part in path.split('.').filter(|s| !s.is_empty()) {
        cur = match cur {
            Value::Object(m) => m.get(part)?,
            Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if s.len() % 2 != 0 {
        return Err("hex 音频长度不是偶数".into());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for i in (0..bytes.len()).step_by(2) {
        let hi = (bytes[i] as char)
            .to_digit(16)
            .ok_or_else(|| "hex 音频含非法字符".to_string())?;
        let lo = (bytes[i + 1] as char)
            .to_digit(16)
            .ok_or_else(|| "hex 音频含非法字符".to_string())?;
        out.push(((hi << 4) | lo) as u8);
    }
    Ok(out)
}

fn from_base64(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.chars() {
        if c.is_whitespace() {
            continue;
        }
        if c == '=' {
            break;
        }
        let v = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            _ => return Err("base64 音频含非法字符".into()),
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Ok(out)
}

/// 猜音频格式（靠文件头）。
pub fn guess_format(bytes: &[u8]) -> &'static str {
    if bytes.len() >= 4 && &bytes[..4] == b"RIFF" {
        "wav"
    } else if bytes.len() >= 3 && &bytes[..3] == b"ID3" {
        "mp3"
    } else if bytes.len() >= 2 && &bytes[..2] == b"ID" {
        "mp3"
    } else if bytes.len() >= 4 && &bytes[..4] == b"OggS" {
        "ogg"
    } else if bytes.len() >= 4 && &bytes[..4] == b"fLaC" {
        "flac"
    } else {
        "wav"
    }
}

/// key 优先级：设置里直接填的 > 进程环境变量 > 用户级环境变量（HKCU）。
fn resolve_key(r: &Resolved) -> String {
    lookup_key(&r.api_key, &r.api_key_env)
}

/// 按同样的优先级找 key。ASR 那条路也用它，两边行为保持一致。
pub fn lookup_key(user_key: &str, env_name: &str) -> String {
    if !user_key.trim().is_empty() {
        return user_key.trim().to_string();
    }
    if env_name.trim().is_empty() {
        return String::new();
    }
    if let Ok(v) = std::env::var(env_name) {
        if !v.trim().is_empty() {
            return v;
        }
    }
    user_env_var(env_name).unwrap_or_default()
}

#[cfg(windows)]
fn user_env_var(name: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = Command::new("reg")
        .args(["query", "HKCU\\Environment", "/v", name])
        .stdin(Stdio::null())
        .creation_flags(0x0800_0000)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
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

/// 播放音频。wav 直接播，其它格式先转成 wav 再播。
/// 返回 Err 时是「播放不了」的原因（合成成功了但放不出来）。
pub fn play(audio: &Audio, tag: &str) -> Result<(), String> {
    let src = std::env::temp_dir().join(format!("codex-pet-play-{tag}.{}", audio.format));
    fs::write(&src, &audio.bytes).map_err(|e| format!("写音频文件失败：{e}"))?;

    // 非 wav 先转：SoundPlayer 只认 wav
    let wav = if audio.format == "wav" {
        src
    } else {
        let dst = std::env::temp_dir().join(format!("codex-pet-play-{tag}.wav"));
        let _ = fs::remove_file(&dst);
        match convert_to_wav(&src, &dst) {
            Ok(()) => {
                let _ = fs::remove_file(&src);
                dst
            }
            Err(e) => {
                let _ = fs::remove_file(&src);
                return Err(format!("{e}（装个 ffmpeg 并加进 PATH 就能放 {} 了）", audio.format));
            }
        }
    };

    let out = play_wav(&wav);
    let _ = fs::remove_file(&wav);
    out
}

/// ffmpeg 转码。ffmpeg 找不到时给出明确提示。
fn convert_to_wav(src: &Path, dst: &Path) -> Result<(), String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-i")
        .arg(src)
        .arg("-ar")
        .arg("32000")
        .arg("-ac")
        .arg("1")
        .arg(dst);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "找不到 ffmpeg".to_string()
        } else {
            format!("调 ffmpeg 失败：{e}")
        }
    })?;
    if !out.status.success() || !dst.exists() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "ffmpeg 转码失败：{}",
            err.lines().next().unwrap_or("").trim()
        ));
    }
    Ok(())
}

/// 用 PowerShell 的 SoundPlayer 播 wav。
/// 走 `-File` 而不是 `-Command`：PowerShell 5.1 会把长 `-Command` 字符串截断。
fn play_wav(path: &Path) -> Result<(), String> {
    const PLAY_SCRIPT: &str = r#"param([Parameter(Mandatory=$true)][string]$WavFile)
$ErrorActionPreference = "Continue"
try {
  $p = New-Object System.Media.SoundPlayer $WavFile
  $p.PlaySync()
  $p.Dispose()
  exit 0
} catch {
  [System.IO.File]::WriteAllText((Join-Path $env:TEMP "codex-pet-tts.err"), ("play: " + $_.Exception.Message))
  exit 1
}
"#;
    let script = std::env::temp_dir().join("codex-pet-play.ps1");
    // 必须带 BOM：PS 5.1 对无 BOM 的 UTF-8 脚本按 ANSI 解码，中文注释会吞掉后面的行
    let body = format!("\u{feff}{PLAY_SCRIPT}");
    let needs_write = fs::read_to_string(&script)
        .map(|s| s.trim_start_matches('\u{feff}') != PLAY_SCRIPT)
        .unwrap_or(true);
    if needs_write {
        fs::write(&script, body).map_err(|e| format!("写播放脚本失败：{e}"))?;
    }

    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ])
    .arg(&script)
    .arg("-WavFile")
    .arg(path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| format!("调 PowerShell 失败：{e}"))?;
    if !out.status.success() {
        let err = std::env::temp_dir().join("codex-pet-tts.err");
        let msg = fs::read_to_string(&err).unwrap_or_default();
        return Err(if msg.trim().is_empty() {
            "播放失败".into()
        } else {
            msg.trim().to_string()
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 预设表覆盖了常见厂商() {
        let ids: Vec<String> = provider_list().into_iter().map(|p| p.id).collect();
        for want in ["minimax", "openai", "azure", "custom", "sapi", "command"] {
            assert!(ids.iter().any(|i| i == want), "缺少预设 {want}");
        }
    }

    #[test]
    fn 预设默认值能填出请求体() {
        let cfg = TtsSettings {
            provider: Some("minimax".into()),
            ..Default::default()
        };
        let r = cfg.resolve();
        let body = build_body(&r, "你好").unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["text"], "你好");
        assert_eq!(v["voice_setting"]["voice_id"], "female-tianmei");
        assert_eq!(v["audio_setting"]["format"], "wav");
    }

    #[test]
    fn openai兼容的请求体格式() {
        let cfg = TtsSettings {
            provider: Some("openai".into()),
            ..Default::default()
        };
        let r = cfg.resolve();
        let body = build_body(&r, "hello").unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["input"], "hello");
        assert_eq!(v["voice"], "nova");
        assert_eq!(v["response_format"], "wav");
    }

    #[test]
    fn 自定义模板的占位符替换() {
        let tpl = r#"{"m":"{{model}}","t":"{{text}}","v":"{{voice}}","s":{{speed}}}"#;
        let out = apply_template(tpl, "你好\"世界", "mm", "vv", "happy", 1.5);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["m"], "mm");
        assert_eq!(v["t"], "你好\"世界");
        assert_eq!(v["v"], "vv");
        assert_eq!(v["s"], 1.5);
    }

    #[test]
    fn 模板占位符能处理特殊字符() {
        let tpl = r#"{"t":"{{text}}"}"#;
        let out = apply_template(tpl, "换行\n制表\t反斜杠\\引号\"", "", "", "", 1.0);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["t"], "换行\n制表\t反斜杠\\引号\"");
    }

    #[test]
    fn azure路径里的模型占位被替换() {
        let cfg = TtsSettings {
            provider: Some("azure".into()),
            endpoint: Some("https://x.openai.azure.com".into()),
            model: Some("my-deploy".into()),
            ..Default::default()
        };
        let r = cfg.resolve();
        let url = format!("{}{}", r.endpoint, r.path.replace("{model}", &r.model));
        assert!(url.contains("my-deploy"), "实际是 {url}");
    }

    #[test]
    fn hex音频能正确解码() {
        let bytes = [0x52u8, 0x49, 0x46, 0x46, 0x00, 0x11, 0x22, 0x33];
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(from_hex(&hex).unwrap(), bytes);
    }

    #[test]
    fn base64音频能正确解码() {
        let bytes = b"RIFFtest";
        assert_eq!(from_base64("UklGRnRlc3Q=").unwrap(), bytes);
    }

    #[test]
    fn json路径能取出嵌套字段() {
        let v = json!({"data": {"audio": "xyz"}});
        assert_eq!(json_path(&v, "data.audio").unwrap().as_str(), Some("xyz"));
        assert!(json_path(&v, "data.missing").is_none());
    }

    #[test]
    fn 音频格式靠文件头判断() {
        assert_eq!(guess_format(b"RIFF...."), "wav");
        assert_eq!(guess_format(b"OggS...."), "ogg");
        assert_eq!(guess_format(b"ID3......"), "mp3");
    }

    #[test]
    fn 音频格式识别错误时给出人话报错() {
        let cfg = TtsSettings {
            provider: Some("openai".into()),
            ..Default::default()
        };
        let r = cfg.resolve();
        let resp = br#"{"error":{"message":"invalid api key"}}"#;
        let err = decode_audio(&r, resp).unwrap_err();
        assert!(err.contains("invalid api key"), "实际是 {err}");
    }

    #[test]
    fn 空音频会报错而不是静默成功() {
        let cfg = TtsSettings {
            provider: Some("minimax".into()),
            ..Default::default()
        };
        let r = cfg.resolve();
        let resp = br#"{"data":{"audio":""}}"#;
        assert!(decode_audio(&r, resp).is_err());
    }

    #[test]
    fn 用户配置覆盖预设默认值() {
        let cfg = TtsSettings {
            provider: Some("minimax".into()),
            model: Some("my-model".into()),
            voice: Some("my-voice".into()),
            endpoint: Some("https://my.api".into()),
            ..Default::default()
        };
        let r = cfg.resolve();
        assert_eq!(r.model, "my-model");
        assert_eq!(r.voice, "my-voice");
        assert_eq!(r.endpoint, "https://my.api");
    }

    #[test]
    fn 旧配置里的voice_id也能用() {
        let cfg = TtsSettings {
            provider: Some("minimax".into()),
            voice_id: Some("old-style".into()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve().voice, "old-style");
    }

    #[test]
    fn 老配置没有provider时默认走minimax() {
        let cfg = TtsSettings {
            voice_id: Some("female-shaonv".into()),
            ..Default::default()
        };
        assert_eq!(cfg.provider_id(), "minimax");
        assert_eq!(cfg.resolve().voice, "female-shaonv");
    }

    #[test]
    fn 外部命令模式拼占位符() {
        let cfg = TtsSettings {
            provider: Some("command".into()),
            command: Some("tts --text {text_file} --out {out_file} --voice {voice}".into()),
            voice: Some("xx".into()),
            ..Default::default()
        };
        let r = cfg.resolve();
        let expanded = r
            .command
            .replace("{text_file}", "T.txt")
            .replace("{out_file}", "O.wav")
            .replace("{voice}", &r.voice);
        assert_eq!(expanded, "tts --text T.txt --out O.wav --voice xx");
    }
}

