// 设置窗口：读写 pet.json。界面只碰自己认识的字段，其它键原样带回，
// 不直接让用户手改 JSON，也就不会因为少个逗号把配置写坏。
const invoke = window.__TAURI__?.core?.invoke;
const listen = window.__TAURI__?.event?.listen;

const $ = (id) => document.getElementById(id);

let cfg = {};
let defaults = {};

function setStatus(text) {
  $("status").textContent = text || "";
}

function fill() {
  const tts = cfg.tts || {};
  const asr = cfg.asr || {};
  const chat = cfg.chat || {};
  const dTts = defaults.tts || {};
  const dAsr = defaults.asr || {};

  $("voice").checked = cfg.voice !== false;
  $("ttsEngine").value = tts.engine === "sapi" ? "sapi" : "minimax";
  // 空值回填出厂默认，表单里显示的就是实际会生效的值
  $("ttsVoice").value = tts.voice_id || dTts.voice_id || "";
  $("ttsModel").value = tts.model || dTts.model || "";
  $("ttsEndpoint").value = tts.endpoint || dTts.endpoint || "";
  $("ttsProxy").value = tts.proxy || "";
  $("ttsKeyEnv").value = tts.api_key_env || dTts.api_key_env || "";
  $("sapiVoice").value = tts.sapi_voice || "";

  $("asrEndpoint").value = asr.endpoint || dAsr.endpoint || "";
  $("asrFfmpeg").value = asr.ffmpeg || "";
  $("asrLanguage").value = asr.language || dAsr.language || "";
  $("asrDevice").value = asr.device || "";

  const command = Array.isArray(chat.command) ? chat.command : [];
  $("chatCommand").value = command.join("\n");
  $("chatCwd").value = chat.cwd || "";
  $("chatEnv").value = Object.entries(chat.env || {})
    .map(([k, v]) => `${k}=${v}`)
    .join("\n");

  $("port").value = cfg.port ?? 8765;
  $("scale").value = String(cfg.scale ?? 1);
  $("alwaysOnTop").checked = cfg.always_on_top !== false;
  if (!$("previewText").value) $("previewText").value = "你好呀，我是 alice，很高兴见到你。";
  autoGrowAll();
}

/// 文本域按内容长高，别在设置页里再冒一条自己的滚动条（超过 220px 才内部滚动）
function autoGrow(el) {
  el.style.height = "auto";
  if (!el.scrollHeight) return;
  el.style.height = `${Math.min(el.scrollHeight + 2, 220)}px`;
}

function autoGrowAll() {
  autoGrow($("chatCommand"));
  autoGrow($("chatEnv"));
}

async function load() {
  if (!invoke) {
    setStatus("没有 Tauri 桥，设置读写不可用");
    return;
  }
  try {
    const data = await invoke("pet_get_config");
    cfg = data.config || {};
    defaults = data.defaults || {};
    fill();
    $("configPath").textContent = `配置文件：${data.path || ""}`;
    setStatus("");
  } catch (err) {
    setStatus(`读取配置失败：${err}`);
  }
}

function collect() {
  const next = structuredClone(cfg && typeof cfg === "object" ? cfg : {});
  next.tts = { ...(next.tts || {}) };
  next.asr = { ...(next.asr || {}) };
  next.chat = { ...(next.chat || {}) };
  next.chat.env = {};

  next.voice = $("voice").checked;
  next.tts.engine = $("ttsEngine").value;
  next.tts.voice_id = $("ttsVoice").value.trim();
  next.tts.model = $("ttsModel").value.trim();
  next.tts.endpoint = $("ttsEndpoint").value.trim();
  next.tts.proxy = $("ttsProxy").value.trim();
  next.tts.api_key_env = $("ttsKeyEnv").value.trim();
  next.tts.sapi_voice = $("sapiVoice").value.trim();

  next.asr.endpoint = $("asrEndpoint").value.trim();
  next.asr.ffmpeg = $("asrFfmpeg").value.trim();
  next.asr.language = $("asrLanguage").value.trim();
  next.asr.device = $("asrDevice").value.trim();

  next.chat.command = $("chatCommand").value
    .split("\n")
    .map((s) => s.trim())
    .filter(Boolean);
  next.chat.cwd = $("chatCwd").value.trim();
  for (const line of $("chatEnv").value.split("\n")) {
    const text = line.trim();
    if (!text) continue;
    const i = text.indexOf("=");
    if (i <= 0) continue;
    next.chat.env[text.slice(0, i).trim()] = text.slice(i + 1).trim();
  }

  const port = Number($("port").value);
  next.port = Number.isFinite(port) && port > 0 ? port : 8765;
  next.scale = Number($("scale").value) || 1;
  next.always_on_top = $("alwaysOnTop").checked;
  return next;
}

async function save() {
  const next = collect();
  if (!next.chat.command.length) {
    setStatus("命令不能为空");
    return;
  }
  $("saveBtn").disabled = true;
  try {
    await invoke("pet_save_config", { config: next });
    cfg = next;
    setStatus("已保存（语音、缩放立即生效；端口和对话命令下次启动生效）");
  } catch (err) {
    setStatus(`保存失败：${err}`);
  } finally {
    $("saveBtn").disabled = false;
  }
}

/// 恢复默认只回填「容易调坏」的部分，对话命令/目录/环境变量保留，
/// 免得把本机 Codex 的绝对路径和代理一起清掉。
function resetDefaults() {
  const kept = {
    command: $("chatCommand").value,
    cwd: $("chatCwd").value,
    env: $("chatEnv").value,
  };
  cfg = { ...structuredClone(defaults), chat: cfg.chat || {} };
  fill();
  $("chatCommand").value = kept.command;
  $("chatCwd").value = kept.cwd;
  $("chatEnv").value = kept.env;
  setStatus("已填入默认值（对话命令保留），点保存生效");
}

function preview() {
  if (!invoke) return;
  const text = $("previewText").value.trim();
  invoke("pet_preview_voice", { text }).catch((err) => setStatus(`试听失败：${err}`));
  setStatus("正在合成试听…（若刚改过音色，先点保存）");
}

function closeWindow() {
  const w = window.__TAURI__?.window?.getCurrentWindow?.();
  if (w) w.hide().catch(() => window.close());
  else window.close();
}

window.addEventListener("DOMContentLoaded", () => {
  load();
  $("saveBtn").addEventListener("click", save);
  $("resetBtn").addEventListener("click", resetDefaults);
  $("closeBtn").addEventListener("click", closeWindow);
  $("previewBtn").addEventListener("click", preview);
  for (const id of ["chatCommand", "chatEnv"]) {
    $(id).addEventListener("input", (e) => autoGrow(e.target));
  }
  if (listen) listen("settings:refresh", load);
});
