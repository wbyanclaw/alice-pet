//! `--mcp` 模式：把桌宠暴露成 MCP server（stdio JSON-RPC）。
//!
//! 由 agent 侧拉起，不建窗口；每次工具调用都转发给已在运行的桌宠进程，
//! 端口从 `%APPDATA%\alice-pet\endpoint.json` 读取，读不到就扫默认区间。

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::{json, Value};

/// 阻塞读 stdin，逐行处理 JSON-RPC。
pub fn serve() -> i32 {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let raw = line.trim();
        if raw.is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(raw) {
            Ok(msg) => handle(msg),
            Err(e) => Some(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": format!("parse error: {e}") }
            })),
        };
        if let Some(v) = reply {
            let mut out = io::stdout();
            let _ = writeln!(out, "{v}");
            let _ = out.flush();
        }
    }
    0
}

fn handle(msg: Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let is_notification = msg.get("id").is_none();
    let id = msg.get("id").cloned().unwrap_or(Value::Null);

    let result: Option<Result<Value, (i64, String)>> = match method.as_str() {
        "initialize" => {
            // 回显客户端的版本，兼容 2024-11-05 到最新
            let proto = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-06-18");
            Some(Ok(json!({
                "protocolVersion": proto,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "alice-pet", "version": env!("CARGO_PKG_VERSION") }
            })))
        }
        "tools/list" => Some(Ok(json!({ "tools": tools() }))),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            Some(match call_tool(name, &args) {
                Ok(text) => Ok(json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": false
                })),
                Err(e) => Ok(json!({
                    "content": [{ "type": "text", "text": e }],
                    "isError": true
                })),
            })
        }
        "ping" => Some(Ok(json!({}))),
        _ => None,
    };

    if is_notification {
        return None;
    }

    Some(match result {
        Some(Ok(v)) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
        Some(Err((code, message))) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        }),
        None => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("unknown method: {method}") }
        }),
    })
}

fn tools() -> Value {
    json!([
        {
            "name": "pet_say",
            "description": "让桌面宠物用气泡对用户说一句话。适合简短播报：任务完成、需要确认、出错提醒。一到两句，不要贴长文。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "要显示的文字" }
                },
                "required": ["text"]
            }
        },
        {
            "name": "pet_gesture",
            "description": "切换桌宠姿态（activity），或播放一次短动作（gesture）。两者可同时给，也可以只给其中一个。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "activity": {
                        "type": "string",
                        "enum": ["idle", "sleeping", "thinking", "writing", "tool", "done", "error"],
                        "description": "目标姿态：思考/写代码/调工具/完成/出错/空闲"
                    },
                    "gesture": {
                        "type": "string",
                        "enum": ["nod", "shake", "lean", "cheer", "sigh", "drowse", "startle"],
                        "description": "一次性动作：点头确认/摇头否定/凑近看/雀跃/松口气/打瞌睡/被吓一跳"
                    },
                    "detail": { "type": "string", "description": "可选的一行说明，短暂显示在宠物上方" }
                },
                "anyOf": [{ "required": ["activity"] }, { "required": ["gesture"] }]
            }
        },
        {
            "name": "pet_state",
            "description": "读取桌宠当前姿态与最近一次事件详情。",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "pet_speak",
            "description": "让桌宠把这句话念出来（Windows 本地语音）。和 pet_say 不同：pet_say 只上屏，pet_speak 只出声。要边显示边朗读就两个都调，或直接用 pet_say（用户在右键菜单里开了语音朗读时会自动念）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "要朗读的文字，建议一到两句" }
                },
                "required": ["text"]
            }
        }
    ])
}

fn call_tool(name: &str, args: &Value) -> Result<String, String> {
    match name {
        "pet_say" => {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("").trim();
            if text.is_empty() {
                return Err("pet_say 需要非空的 text".into());
            }
            http("POST", "/say", Some(&json!({ "text": text }).to_string()))?;
            Ok(format!("已显示气泡：{text}"))
        }
        "pet_gesture" => {
            let activity = args.get("activity").and_then(Value::as_str).unwrap_or("");
            let gesture = args.get("gesture").and_then(Value::as_str).unwrap_or("");
            if !activity.is_empty()
                && !matches!(
                    activity,
                    "idle" | "sleeping" | "thinking" | "writing" | "tool" | "done" | "error"
                )
            {
                return Err(format!("activity 不合法：{activity}"));
            }
            if !gesture.is_empty()
                && !matches!(
                    gesture,
                    "nod" | "shake" | "lean" | "cheer" | "sigh" | "drowse" | "startle"
                )
            {
                return Err(format!("gesture 不合法：{gesture}"));
            }
            if activity.is_empty() && gesture.is_empty() {
                return Err("pet_gesture 需要 activity 或 gesture".into());
            }
            let mut body = json!({});
            if !activity.is_empty() {
                let detail = args.get("detail").and_then(Value::as_str).unwrap_or("mcp");
                body["activity"] = json!(activity);
                body["detail"] = json!(detail);
            }
            if !gesture.is_empty() {
                body["gesture"] = json!(gesture);
            }
            http("POST", "/event", Some(&body.to_string()))?;
            let mut done = Vec::new();
            if !activity.is_empty() {
                done.push(format!("姿态已切到 {activity}"));
            }
            if !gesture.is_empty() {
                done.push(format!("动作 {gesture}"));
            }
            Ok(done.join("，"))
        }
        "pet_state" => http("GET", "/state", None),
        "pet_speak" => {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("").trim();
            if text.is_empty() {
                return Err("pet_speak 需要非空的 text".into());
            }
            http("POST", "/speak", Some(&json!({ "text": text }).to_string()))?;
            Ok(format!("正在朗读：{text}"))
        }
        other => Err(format!("未知工具：{other}")),
    }
}

/// 找到桌宠端口后发一个请求。
fn http(method: &str, path: &str, body: Option<&str>) -> Result<String, String> {
    let port = endpoint_port()?;
    http_to(port, method, path, body)
}

fn http_to(port: u16, method: &str, path: &str, body: Option<&str>) -> Result<String, String> {
    let addr = format!("127.0.0.1:{port}");
    let mut stream = TcpStream::connect(&addr).map_err(|e| format!("桌宠没在运行（{addr}）：{e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let payload = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&resp).to_string();
    let (head, payload) = match text.split_once("\r\n\r\n") {
        Some((h, b)) => (h, b),
        None => (text.as_str(), ""),
    };
    if !head.starts_with("HTTP/1.1 2") {
        return Err(format!("桌宠返回：{head}"));
    }
    Ok(payload.to_string())
}

/// 端口发现：环境变量 → endpoint.json → 扫默认区间。
fn endpoint_port() -> Result<u16, String> {
    if let Ok(p) = std::env::var("PET_PORT") {
        if let Ok(p) = p.parse::<u16>() {
            return Ok(p);
        }
    }
    if let Ok(s) = std::fs::read_to_string(crate::endpoint_path()) {
        if let Ok(v) = serde_json::from_str::<Value>(&s) {
            if let Some(p) = v.get("port").and_then(Value::as_u64) {
                let p = p as u16;
                if http_to(p, "GET", "/state", None).is_ok() {
                    return Ok(p);
                }
            }
        }
    }
    for p in crate::DEFAULT_PORT..crate::DEFAULT_PORT + 10 {
        if http_to(p, "GET", "/state", None).is_ok() {
            return Ok(p);
        }
    }
    Err("没找到运行中的桌宠：请先启动 alice-pet.exe".into())
}
