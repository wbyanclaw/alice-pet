// 设置窗口：读写 pet.json。界面只碰自己认识的字段，其它键原样带回，
// 不直接让用户手改 JSON，也就不会因为少个逗号把配置写坏。
const invoke = window.__TAURI__?.core?.invoke;
const listen = window.__TAURI__?.event?.listen;

const $ = (id) => document.getElementById(id);

let cfg = {};
let defaults = {};
let providers = [];

function setStatus(text) {
  $("status").textContent = text || "";
}

// ---------- 语音服务预设 ----------

/// 找出当前选中的供应商预设
function currentProvider() {
  const id = $("ttsProvider").value;
  return providers.find((p) => p.id === id) || null;
}

/// 切换供应商时把对应字段的默认值填进去。
/// 换了供应商就整份重来——旧供应商的地址/路径对新供应商是错的，
/// 留着比留空更坑。用户改过的值在「重新选同一供应商」时不受影响。
function applyProviderDefaults(p, force = false) {
  if (!p) return;
  const set = (id, val) => {
    const el = $(id);
    if (!el) return;
    if (force || !el.value.trim()) el.value = val ?? "";
  };
  set("ttsEndpoint", p.endpoint);
  set("ttsPath", p.path);
  set("ttsModel", p.model);
  set("ttsVoice", p.voice);
  set("ttsKeyEnv", p.apiKeyEnv);
  set("ttsAudioPath", p.audioPath);
  set("ttsAudioFormat", p.audioFormat);
}

/// 按供应商类型显隐相关字段——不相关的字段藏起来，界面才不啰嗦
function updateVisibility() {
  const p = currentProvider();
  if (!p) return;
  const show = {
    cloud: !p.local,
    key: p.needsKey,
    custom: p.id === "custom",
    sapi: p.id === "sapi",
    command: p.id === "command",
  };
  for (const el of document.querySelectorAll("[data-when]")) {
    const mode = el.getAttribute("data-when");
    el.style.display = show[mode] ? "" : "none";
  }
  $("ttsHint").textContent = p.hint || "";
}

/// 按识别引擎显隐字段
function updateAsrVisibility() {
  const id = $("asrProvider").value;
  const show = {
    http: id === "whisper",
    openai: id === "openai",
    command: id === "command",
  };
  for (const el of document.querySelectorAll("[data-asr]")) {
    const mode = el.getAttribute("data-asr");
    el.style.display = show[mode] ? "" : "none";
  }
}

/// 载入供应商下拉框
async function loadProviders() {
  if (!invoke) return;
  try {
    const data = await invoke("pet_tts_providers");
    providers = data.providers || [];
  } catch {
    providers = [];
  }
  const sel = $("ttsProvider");
  sel.innerHTML = "";
  for (const p of providers) {
    const opt = document.createElement("option");
    opt.value = p.id;
    opt.textContent = p.label;
    sel.appendChild(opt);
  }
  if (!providers.length) {
    const opt = document.createElement("option");
    opt.value = "";
    opt.textContent = "（没拿到预设列表）";
    sel.appendChild(opt);
  }
}

function fill() {
  const tts = cfg.tts || {};
  const asr = cfg.asr || {};
  const chat = cfg.chat || {};
  const dTts = defaults.tts || {};
  const dAsr = defaults.asr || {};

  $("voice").checked = cfg.voice !== false;

  // 语音服务：优先用配置里的 provider，老配置里的 engine 字段也认
  const pid = tts.provider || tts.engine || dTts.provider || "minimax";
  if ($("ttsProvider").options.length) $("ttsProvider").value = pid;
  const p = currentProvider();
  applyProviderDefaults(p);

  // 回填顺序：用户配置 > 选中供应商的预设 > 出厂默认。
  // 表单里显示的就是实际会生效的值（后端 resolve 也是这个优先级）。
  $("ttsEndpoint").value = tts.endpoint || p?.endpoint || dTts.endpoint || "";
  $("ttsPath").value = tts.path || p?.path || dTts.path || "";
  $("ttsModel").value = tts.model || p?.model || dTts.model || "";
  $("ttsVoice").value = tts.voice || tts.voice_id || p?.voice || dTts.voice || "";
  $("ttsApiKey").value = tts.api_key || "";
  $("ttsKeyEnv").value = tts.api_key_env || p?.apiKeyEnv || dTts.api_key_env || "";
  $("ttsHeaders").value = Object.entries(tts.headers || {})
    .map(([k, v]) => `${k}: ${v}`)
    .join("\n");
  $("ttsAudioPath").value = tts.audio_path || p?.audioPath || dTts.audio_path || "";
  $("ttsAudioFormat").value = tts.audio_format || p?.audioFormat || dTts.audio_format || "hex";
  $("ttsProxy").value = tts.proxy || "";
  $("ttsBodyTemplate").value = tts.body_template || "";
  $("ttsCommand").value = tts.command || "";
  $("sapiVoice").value = tts.sapi_voice || "";

  $("asrProvider").value = asr.provider || dAsr.provider || "whisper";
  $("asrEndpoint").value = asr.endpoint || dAsr.endpoint || "";
  $("asrApiEndpoint").value = asr.api_endpoint || dAsr.api_endpoint || "";
  $("asrModel").value = asr.model || "";
  $("asrApiKey").value = asr.api_key || "";
  $("asrKeyEnv").value = asr.api_key_env || "";
  $("asrCommand").value = asr.command || "";
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
  updateVisibility();
  updateAsrVisibility();
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
  autoGrow($("ttsBodyTemplate"));
}

async function load() {
  if (!invoke) {
    setStatus("没有 Tauri 桥，设置读写不可用");
    return;
  }
  try {
    await loadProviders();
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
  next.tts.provider = $("ttsProvider").value;
  // 旧字段名一起留着，老版本读到也不会报错
  next.tts.engine = $("ttsProvider").value;
  next.tts.endpoint = $("ttsEndpoint").value.trim();
  next.tts.path = $("ttsPath").value.trim();
  next.tts.model = $("ttsModel").value.trim();
  next.tts.voice = $("ttsVoice").value.trim();
  next.tts.voice_id = $("ttsVoice").value.trim();
  next.tts.api_key = $("ttsApiKey").value.trim();
  next.tts.api_key_env = $("ttsKeyEnv").value.trim();
  next.tts.audio_path = $("ttsAudioPath").value.trim();
  next.tts.audio_format = $("ttsAudioFormat").value;
  next.tts.proxy = $("ttsProxy").value.trim();
  next.tts.body_template = $("ttsBodyTemplate").value.trim();
  next.tts.command = $("ttsCommand").value.trim();
  next.tts.sapi_voice = $("sapiVoice").value.trim();

  // 额外请求头：每行 KEY: VALUE
  next.tts.headers = {};
  for (const line of $("ttsHeaders").value.split("\n")) {
    const text = line.trim();
    if (!text) continue;
    const i = text.indexOf(":");
    if (i <= 0) continue;
    next.tts.headers[text.slice(0, i).trim()] = text.slice(i + 1).trim();
  }

  next.asr.provider = $("asrProvider").value;
  next.asr.endpoint = $("asrEndpoint").value.trim();
  next.asr.api_endpoint = $("asrApiEndpoint").value.trim();
  next.asr.model = $("asrModel").value.trim();
  next.asr.api_key = $("asrApiKey").value.trim();
  next.asr.api_key_env = $("asrKeyEnv").value.trim();
  next.asr.command = $("asrCommand").value.trim();
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

async function preview() {
  if (!invoke) return;
  const text = $("previewText").value.trim();
  $("ttsStatus").textContent = "正在合成试听…";
  $("previewBtn").disabled = true;
  try {
    // 用界面上的当前值试听，不落盘——改完没保存也能先听效果
    const probe = collect();
    await invoke("pet_preview_voice", { config: probe, text });
    $("ttsStatus").textContent = "试听成功";
  } catch (err) {
    $("ttsStatus").textContent = `试听失败：${err}`;
  } finally {
    $("previewBtn").disabled = false;
  }
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
  for (const id of ["chatCommand", "chatEnv", "ttsBodyTemplate"]) {
    $(id).addEventListener("input", (e) => autoGrow(e.target));
  }
  // 换供应商：整份换成新供应商的默认值 + 切换字段显隐
  $("ttsProvider").addEventListener("change", () => {
    applyProviderDefaults(currentProvider(), true);
    updateVisibility();
    $("ttsStatus").textContent = "";
  });
  // 换识别引擎：切换字段显隐
  $("asrProvider").addEventListener("change", updateAsrVisibility);
  if (listen) listen("settings:refresh", load);
});
