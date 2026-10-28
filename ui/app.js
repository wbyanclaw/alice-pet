// Codex 桌宠前端：按状态切雪碧、气泡、输入框、语音输入与右键菜单。
// 图集是 4x4 共 16 帧，逐帧播放交给 CSS 的 steps(1) 动画（见 style.css），
// 前端只负责切状态 class，帧的节奏由各状态的 --sheet 与动画时长决定。
// 素材帧间取景漂移只有 1~2px（256px 画布），缩到 200px 后不足 1.5px，不可见，
// 所以不需要额外的取景补偿。

// 可一次性播放的短动作，交给 CSS 关键帧
const GESTURES = ["nod", "shake", "lean", "cheer", "sigh", "drowse", "startle"];

// 状态 class 名。和 style.css 里的 #pet.<状态> 一一对应，
// 换图集、换播放时长都由 CSS 处理，这里只负责加/减 class。
const STATES = ["idle", "sleeping", "thinking", "writing", "tool", "done", "error"];

const ACTIVITY_LABEL = {
  idle: "空闲",
  sleeping: "睡着了",
  thinking: "思考中",
  writing: "写字中",
  tool: "调工具中",
  done: "刚完成",
  error: "出错了",
};

const SCALES = [0.8, 1.0, 1.2];
const PETTED_MS = 1600;
const SPEAKING_MS = 4000;
const FIDGET_MIN = 14000; // 空闲时随机做小动作的间隔
const FIDGET_MAX = 32000;
const CLICK_SLOP = 6; // 按下与抬起位移超过这个值就当作拖动，不弹输入框

const pet = document.getElementById("pet");
const bubble = document.getElementById("bubble");
const composer = document.getElementById("composer");
const promptInput = document.getElementById("prompt");
const menu = document.getElementById("menu");
const voiceMark = document.getElementById("voiceMark");
const topMark = document.getElementById("topMark");
const scaleMark = document.getElementById("scaleMark");
const listenLabel = document.getElementById("listenLabel");
const listenMark = document.getElementById("listenMark");
const dot = document.getElementById("dot");

const tauri = window.__TAURI__;
const invoke = tauri?.core?.invoke;
const listen = tauri?.event?.listen;

let activity = "idle";
let detail = "";
let bubbleTimer = null;
let sending = false;
let voiceOn = false;
let topmost = true;
let scale = 1;
let listening = false;
let dragging = false;
let speaking = false;
let speakingTimer = null;

/// 图集切换由 CSS 变量 --sheet 控制：状态 class 一换，背景图跟着换。
/// 拖拽时用 dragging 图集，由 body.dragging 覆盖（style.css 里在状态规则之后）。
function apply(activityName, newDetail) {
  activity = activityName;
  detail = newDetail || "";
  stopSpeaking();
  // 只换状态 class，别把说话/被摸头这些临时 class 一起抹掉
  pet.classList.remove(...STATES);
  pet.classList.add(activityName);
}

// ---------- 拖拽 ----------

/// 一按下就进入"被拎起来"的姿势：类名立刻生效（动画走合成器）。
function beginDragVisual() {
  if (dragging) return;
  dragging = true;
  document.body.classList.add("dragging");
}

function endDragVisual() {
  if (!dragging) return;
  dragging = false;
  document.body.classList.remove("dragging");
}

/// 窗口拖动交给原生实现才跟手；startDragging 的 Promise 在拖拽结束时才 resolve，
/// 正好用来收掉姿势。所以 #pet 上的 data-tauri-drag-region 要写 false 避免重复触发。
async function dragWindow() {
  const win = tauri?.window?.getCurrentWindow?.();
  if (!win || !win.startDragging) return;
  beginDragVisual();
  try {
    await win.startDragging();
  } catch (err) {
    /* 拖动失败就退回普通点击，不打断交互 */
  } finally {
    endDragVisual();
  }
}

/// 播放一次性动作，不改当前状态（动作结束后回到状态的循环动画）
function playGesture(name) {
  if (!GESTURES.includes(name)) return;
  pet.classList.remove(...GESTURES.map((g) => `g-${g}`));
  void pet.offsetWidth; // 强制重排，让同名动作能重新触发
  pet.classList.add(`g-${name}`);
  clearTimeout(playGesture.timer);
  playGesture.timer = setTimeout(() => pet.classList.remove(`g-${name}`), 2400);
}

/// 被摸一下的回应：换成笑脸图集蹦一下，结束后回到当前状态
function reactToPetting() {
  clearTimeout(reactToPetting.timer);
  pet.classList.remove(...GESTURES.map((g) => `g-${g}`));
  void pet.offsetWidth;
  pet.classList.add("petting");
  reactToPetting.timer = setTimeout(() => {
    pet.classList.remove("petting");
    pet.classList.remove(...STATES);
    pet.classList.add(activity);
  }, PETTED_MS);
}

/// 说话中：借 done 图集里带嘴型的帧，配轻微起伏，念完自己停
function startSpeaking(ms = SPEAKING_MS) {
  clearTimeout(speakingTimer);
  speaking = true;
  pet.classList.remove(...GESTURES.map((g) => `g-${g}`));
  pet.classList.add("speaking");
  speakingTimer = setTimeout(stopSpeaking, ms);
}

function stopSpeaking() {
  if (!speaking) return;
  speaking = false;
  clearTimeout(speakingTimer);
  pet.classList.remove("speaking");
}

/// `pinned` 为真时气泡一直挂着（录着音、正在识别），不显示流式光标也不自动消失
function showBubble(text, done, pinned) {
  bubble.textContent = text;
  bubble.classList.add("show");
  bubble.classList.toggle("streaming", !done && !pinned);
  clearTimeout(bubbleTimer);
  if (done) {
    bubbleTimer = setTimeout(() => bubble.classList.remove("show"), 14000);
  }
}

// ---------- 输入框 ----------

function openComposer() {
  if (listening) return; // 录音时别抢输入框
  closeMenu();
  composer.classList.add("show");
  if (tauri?.window?.getCurrentWindow) {
    tauri.window.getCurrentWindow().setFocus().catch(() => {});
  }
  setTimeout(() => promptInput.focus(), 30);
}

function closeComposer() {
  composer.classList.remove("show");
  promptInput.value = "";
}

function sendText(text) {
  const body = (text || "").trim();
  if (!body || sending) return;
  if (!invoke) {
    showBubble("没有 Tauri 桥，无法发消息", true);
    return;
  }
  sending = true;
  showBubble("…", false);
  invoke("pet_chat", { text: body })
    .catch((err) => showBubble(`发送失败：${err}`, true))
    .finally(() => {
      sending = false;
      closeComposer();
    });
}

function sendPrompt() {
  sendText(promptInput.value);
}

// ---------- 语音输入 ----------

async function startListening() {
  closeMenu();
  closeComposer();
  try {
    await invoke("pet_voice_start");
    setListening(true);
    showBubble("我在听…说完点我一下", false, true);
  } catch (err) {
    showBubble(`开不了麦：${err}`, true);
  }
}

async function stopListening(send) {
  if (!listening) return;
  setListening(false);
  if (!send) {
    if (invoke) invoke("pet_voice_cancel").catch(() => {});
    showBubble("好，不录了", true);
    return;
  }
  showBubble("正在听写…", false, true);
  try {
    const text = await invoke("pet_voice_stop");
    if (text && text.trim()) {
      showBubble(text.trim(), false, true);
      sendText(text);
    } else {
      showBubble("没听清，再说一次？", true);
    }
  } catch (err) {
    showBubble(String(err), true);
  }
}

function setListening(on) {
  listening = on;
  document.body.classList.toggle("listening", on);
  if (listenLabel) listenLabel.textContent = on ? "停止并发送" : "说话";
  if (listenMark) listenMark.textContent = on ? "●" : "";
}

// ---------- 右键菜单 ----------

function openMenu(x, y) {
  menu.classList.add("show");
  const rect = menu.getBoundingClientRect();
  const w = window.innerWidth;
  const h = window.innerHeight;
  let left = x;
  let top = y;
  if (left + rect.width > w - 4) left = Math.max(4, w - rect.width - 4);
  if (top + rect.height > h - 4) top = Math.max(4, y - rect.height);
  menu.style.left = `${left}px`;
  menu.style.top = `${top}px`;
}

function closeMenu() {
  menu.classList.remove("show");
}

function syncMarks() {
  voiceMark.textContent = voiceOn ? "✓" : "";
  topMark.textContent = topmost ? "✓" : "";
  scaleMark.textContent = `${Math.round(scale * 100)}%`;
}

async function toggleVoice() {
  voiceOn = !voiceOn;
  syncMarks();
  if (invoke) {
    try {
      await invoke("pet_set_voice", { enabled: voiceOn });
      showBubble(voiceOn ? "以后回复会自动念出来" : "不念了，只看文字", true);
    } catch (err) {
      showBubble(`切换失败：${err}`, true);
    }
  }
}

async function toggleTopmost() {
  topmost = !topmost;
  syncMarks();
  if (invoke) {
    try {
      await invoke("pet_set_topmost", { enabled: topmost });
      showBubble(topmost ? "我会一直待在最前面" : "不再压着别的窗口了", true);
    } catch (err) {
      showBubble(`切换失败：${err}`, true);
    }
  }
}

async function cycleScale() {
  const i = SCALES.indexOf(scale);
  scale = SCALES[(i + 1) % SCALES.length];
  applyScale(scale);
  syncMarks();
  if (invoke) {
    invoke("pet_set_scale", { scale }).catch((err) => showBubble(`缩放失败：${err}`, true));
  }
}

function applyScale(s) {
  scale = s;
  document.body.style.zoom = String(s);
  if (scaleMark) scaleMark.textContent = `${Math.round(s * 100)}%`;
}

function hidePet() {
  closeMenu();
  closeComposer();
  document.body.classList.add("hidden-pet");
  // 光靠 CSS 隐藏的话透明窗口仍在吞点击，让主进程把窗口一起收进托盘
  if (invoke) invoke("pet_hide_window").catch(() => {});
}

function showPet() {
  document.body.classList.remove("hidden-pet");
}

function showState() {
  const label = ACTIVITY_LABEL[activity] || activity;
  const lines = [`状态：${label}${detail ? `（${detail}）` : ""}`];
  lines.push(`自动念回复：${voiceOn ? "开" : "关"}`);
  lines.push(`总在最前：${topmost ? "开" : "关"}　大小：${Math.round(scale * 100)}%`);
  showBubble(lines.join("\n"), true);
}

async function runMenuAction(act) {
  closeMenu();
  switch (act) {
    case "listen":
      if (listening) await stopListening(true);
      else await startListening();
      break;
    case "chat":
      openComposer();
      break;
    case "voice":
      await toggleVoice();
      break;
    case "top":
      await toggleTopmost();
      break;
    case "scale":
      await cycleScale();
      break;
    case "settings":
      if (invoke) invoke("pet_open_settings").catch((err) => showBubble(`打开设置失败：${err}`, true));
      break;
    case "state":
      showState();
      break;
    case "hide":
      hidePet();
      break;
    case "quit":
      if (invoke) {
        invoke("pet_quit").catch(() => {});
      } else {
        window.close();
      }
      break;
    default:
      break;
  }
}

menu.addEventListener("click", (e) => {
  const item = e.target.closest(".item");
  if (item) runMenuAction(item.dataset.act);
});

document.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  if (document.body.classList.contains("hidden-pet")) return;
  // 再次右键就收起，避免"打开了关不掉"
  if (menu.classList.contains("show")) {
    closeMenu();
    return;
  }
  openMenu(e.clientX, e.clientY);
});

document.addEventListener("mousedown", (e) => {
  if (e.button !== 0) return; // 右键交给 contextmenu 的开关逻辑，别先关再开
  if (!menu.classList.contains("show")) return;
  if (!menu.contains(e.target)) closeMenu();
});

// 有些按在拖动区的 mousedown 会被窗口拖动吃掉，抬起时再兜一次
document.addEventListener("mouseup", (e) => {
  if (e.button !== 0) return;
  if (!menu.classList.contains("show")) return;
  if (!menu.contains(e.target)) closeMenu();
});

// Esc 关菜单/输入框；录音中按 Esc 取消录音
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    if (listening) {
      stopListening(false);
      return;
    }
    closeMenu();
    closeComposer();
  }
});

window.addEventListener("blur", () => {
  closeMenu();
});

// ---------- 鼠标 ----------

// 拖拽和点击共用左键：位移小于阈值才算点击
let downAt = null;
pet.addEventListener("mousedown", (e) => {
  if (e.button !== 0) return;
  downAt = { x: e.screenX, y: e.screenY };
  dragWindow();
});
pet.addEventListener("mouseup", (e) => {
  if (!downAt) return;
  const moved = Math.hypot(e.screenX - downAt.x, e.screenY - downAt.y);
  downAt = null;
  endDragVisual();
  if (moved > CLICK_SLOP) return; // 拖窗口，不触发点击
  if (listening) {
    stopListening(true);
    return;
  }
  reactToPetting();
  if (composer.classList.contains("show")) closeComposer();
  else openComposer();
});

// 原生拖动偶尔会吞掉抬起事件，兜底把姿势收回来
document.addEventListener("mouseup", endDragVisual);
window.addEventListener("blur", endDragVisual);

dot.addEventListener("click", showPet);

promptInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    sendPrompt();
  } else if (e.key === "Escape") {
    closeComposer();
  }
});

// ---------- 启动 ----------

/// 空闲久了随机来个小动作，别一直僵着
function scheduleFidget() {
  clearTimeout(scheduleFidget.timer);
  const wait = FIDGET_MIN + Math.random() * (FIDGET_MAX - FIDGET_MIN);
  scheduleFidget.timer = setTimeout(() => {
    // 说话或被摸头时别插小动作，免得把嘴型和笑脸顶掉
    if (
      activity === "idle" &&
      !composer.classList.contains("show") &&
      !listening &&
      !speaking &&
      !pet.classList.contains("petting")
    ) {
      const pool = ["nod", "lean", "sigh", "shake", "startle"];
      playGesture(pool[Math.floor(Math.random() * pool.length)]);
    }
    scheduleFidget();
  }, wait);
}

window.addEventListener("DOMContentLoaded", () => {
  apply("idle", "");
  syncMarks();
  scheduleFidget();

  if (!listen) {
    showBubble("没有 Tauri 桥，界面不完整", true);
    return;
  }

  if (invoke) {
    invoke("pet_ui_state")
      .then((s) => {
        voiceOn = !!s?.voice;
        topmost = s?.alwaysOnTop !== false;
        applyScale(s?.scale || 1);
        syncMarks();
      })
      .catch(() => {});
  }

  listen("pet:state", (event) => {
    const payload = event.payload || {};
    apply(payload.activity || "idle", payload.detail || "");
  });
  listen("pet:bubble", (event) => {
    const payload = event.payload || {};
    showBubble(payload.text || "", !!payload.done);
  });
  // 朗读时切「说话」姿态，念完恢复
  listen("pet:speaking", (event) => {
    if ((event.payload || {}).on) startSpeaking();
    else stopSpeaking();
  });
  // 外部 agent 通过 MCP / HTTP 触发的单次动作
  listen("pet:gesture", (event) => {
    playGesture((event.payload || {}).gesture || "");
  });
  // 录音开关（菜单点「说话」或主进程主动停）
  listen("pet:listening", (event) => {
    setListening(!!(event.payload || {}).on);
  });
  // 缩放（启动时主进程也可能推一次）
  listen("pet:scale", (event) => {
    const s = Number(event.payload);
    if (Number.isFinite(s) && s > 0) {
      applyScale(s);
      syncMarks();
    }
  });
  // 托盘菜单 / Ctrl+Alt+P 唤回时主进程会先 show 窗口，再通知前端撤掉隐藏样式
  listen("pet:show", showPet);
});
