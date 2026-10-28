# codex-pet

Codex CLI 的桌面宠物：只读 `~/.codex/sessions/**/*.jsonl`，把会话事件映射成状态，用 4×4 雪碧播放动画。

素材与 `dsh-workpet-alice` 同源（Mori `alice_half` 雪碧），窗口 320×380（逻辑像素，可缩放 80%–120%）、透明、无边框、置顶、不进任务栏。

## 运行

```powershell
cargo build --release --manifest-path src-tauri/Cargo.toml
.\src-tauri\target\release\codex-pet.exe
```

**必须用 release 运行**：debug 构建是 console 子系统，会带一个 cmd 窗口；release 是 GUI 子系统，不会出现控制台。

同一个 exe 带 `--mcp` 运行时不建窗口，只做 stdio MCP server（由 agent 拉起，见下文）。

首次编译会拉 Tauri 依赖，比较慢；之后增量编译几秒。

想换监听目录（例如用沙箱 CODEX_HOME）：

```powershell
$env:CODEX_PET_SESSIONS = "d:\POC\chatbot\work\codex-home\sessions"
.\src-tauri\target\release\codex-pet.exe
```

默认监听 `%USERPROFILE%\.codex\sessions`——也就是你自己 CLI 会话的日志，不是生图服务那份沙箱副本。

窗口内任意位置按住拖动；单击角色会播 `done` 姿势 1.6 秒。拖拽依赖 `capabilities/default.json` 里的 `core:window:allow-start-dragging`，删掉它窗口就拖不动了。

## 状态映射

| activity | 触发 | 雪碧 |
|---|---|---|
| `thinking` | `task_started`，或工具刚结束 | thinking |
| `writing` | `token_count` 且无待完成工具调用 | recording |
| `tool` | `function_call` / `custom_tool_call` / `web_search_call` | walking |
| `done` | `task_complete`（2.6 秒后回 idle） | done |
| `error` | `turn_aborted`（2.6 秒后回 idle） | error |
| `idle` | 45 秒无事件 | idle |
| `sleeping` | 180 秒无事件 | sleeping |

点击宠物会切到 `done` 姿势 1.6 秒并打开输入框；拖动超过 6 px 视为拖窗口，不会弹输入框。
拖拽期间换成 `dragging` 姿势、从头顶吊着左右晃，影子同步收小；松手立刻回当前状态。
实现上 `#pet` 的 `data-tauri-drag-region` 写成 `false`，由前端调 `startDragging()` 发起原生拖动，
用它的 Promise（拖完才 resolve）收尾姿势，避免拖动中的姿势卡住。

## 动画

雪碧是 4×4 共 16 帧（1024×1024），**逐帧播放**：CSS 用 `steps(1)` 关键帧在 16 个帧位之间整帧跳，
位移走 `transform`（合成器线程），不重绘背景。各状态的播放时长取自 Mori 的 `manifest.json`：

| 状态 | 时长 | 帧数 | 备注 |
|---|---|---|---|
| idle | 3.0s | 16 | 循环 |
| sleeping | 5.0s | 16 | 循环 |
| thinking | 2.0s | 16 | 循环 |
| writing（recording） | 1.5s | 16 | 循环 |
| tool（walking） | 1.5s | 16 | 循环 |
| done | 1.2s | 16 | **一次性**，播完停在最后一帧 |
| error | 1.6s | 16 | **一次性**，播完停在最后一帧 |
| dragging | 1.6s | 16 | 循环 |

图集切换靠 CSS 变量 `--sheet`（`#pet` 上），状态 class 一换背景就跟着换，
`body.dragging` 在状态规则之后覆盖成 `dragging` 图集。

额外的临时姿态（class 加在 `#pet` 上，优先级高于状态和随机小动作）：

| class | 触发 | 效果 |
|---|---|---|
| `speaking` | TTS 开始朗读 | 借 `done` 图集里带嘴型的帧，配轻微上下起伏，念完自动停 |
| `petting` | 单击角色 | 换成笑脸图集蹦一下，1.6 秒后回到当前状态 |

「说话」姿态由主进程在朗读前后发 `pet:speaking` 事件驱动（`main.rs` 的 `speak()`）。

关于取景漂移：实测相邻帧的像素位移只有 1~2 px（256 px 画布上，缩到 200 px 显示后不足 1.5 px），
`recording` / `sleeping` 完全静止，`dragging` / `walking` 的 ±4 px 是有意的摆动动作。
所以直接连播即可，不需要额外的取景补偿。

## 右键菜单

在宠物上右键弹出菜单（菜单在窗口内会自适应，不会超出边界）：

| 菜单项 | 作用 |
|---|---|
| 说话 | 语音输入：开始/停止录音，停止后走 whisper 听写并直接发出去 |
| 对话 | 打开底部输入框，等价于左键点击 |
| 自动念回复 | 开关 TTS（带 ✓），状态写回 `pet.json` 的 `voice`，下次启动保持不变 |
| 总在最前 | 开关置顶（带 ✓），写回 `always_on_top` |
| 大小 | 在 80% / 100% / 120% 之间循环，写回 `scale` |
| 设置… | 打开图形设置窗口（不再用记事本改 JSON） |
| 状态 | 气泡显示当前姿态、语音开关、置顶与缩放 |
| 隐藏 | 窗口整个收进托盘（透明区域不再挡鼠标），按下面的方式唤回 |
| 退出 | 关掉桌宠进程 |

录音中再点宠物本体 = 停止并发送，按 `Esc` = 取消录音。
实现是前端自绘的 div（Tauri 原生菜单对透明窗口支持不好），见 `ui/app.js` 的 `openMenu`。

## 隐藏后怎么唤回

隐藏是调 `win.hide()` 真把窗口藏起来，同时托盘常驻，所以有三条召回路径：

| 方式 | 说明 |
|---|---|
| 托盘图标左键单击 | 立即显示宠物并抢焦点 |
| 托盘右键 → 显示 alice | 同上，菜单里也有「退出」 |
| 全局热键 `Ctrl+Alt+P` | 任何程序在前台都生效；被别的软件占用时只在 stderr 记一条，不影响托盘召回 |

唤回时主进程会发 `pet:show` 事件，前端收到后撤掉 `hidden-pet` 样式（否则窗口显示出来了宠物还是透明的）。
热键常量在 `src-tauri/src/main.rs` 的 `HOTKEY`，托盘图标用 `tauri.conf.json` 里的 `icons/alice.ico`
（从雪碧第 0 帧裁出的圆形头像，16–256 共 9 档，与屏幕上的角色是同一张脸）。

## 语音

两条链路：**说给桌宠听**（语音输入 → 文本 → 发消息）和**听桌宠说**（TTS 念回复）。

### 语音输入

右键「说话」开始录音（角色外圈出现蓝色呼吸环），再点一次或点角色本体 = 停止并发送，
按 `Esc` = 取消。流程：`ffmpeg` 录麦克风 → 本机 `whisper.cpp` server 听写 → 文本直接当消息发出去。

配置在 `pet.json` 的 `asr` 块：

```json
"asr": {
  "endpoint": "http://127.0.0.1:8080/inference", // whisper.cpp server
  "ffmpeg": "",                                   // 留空自动找：exe 上层目录的 ffmpeg.exe / tools/ffmpeg.exe
  "language": "zh",
  "device": ""                                    // 留空自动取第一个 dshow 录音设备
}
```

`whisper-server` 需要先跑起来（`whisper-server -m ggml-small.bin --language zh -t 8`），
桌宠自己不会拉起它；服务不通时气泡会直接报错。录音参数固定 16 kHz / 16 bit / 单声道，
停止时向 ffmpeg stdin 写 `q` 让 WAV 头写完整；不足 3 KB 直接判「录音太短」。

静音时 whisper 会幻听「謝謝大家」「(字幕:J Chong)」这类字幕客套话，代码里有整句黑名单 +
括号幻听行过滤，命中的直接当「没听清」丢掉，不会发出去。

### 听桌宠说（TTS）

开关打开后，所有 `done` 的气泡都会被念出来（流式中间态只上屏不出声，超过 220 字只念开头）。
可以走三条路触发：

- 右键菜单「自动念回复」——全局开关，`pet.json` 里存 `voice: true/false`
- MCP 工具 `pet_speak`——agent 直接让它念一句，不管开关状态
- `POST /speak`——外部脚本让它出声（与 `/say` 的区别是只出声、不起气泡）

合成默认走**云端 MiniMax**（系统没装中文语音包也能念中文），失败自动回退本地 SAPI。
配置在 `pet.json` 的 `tts` 块：

```json
"tts": {
  "engine": "minimax",              // minimax（默认）或 sapi
  "endpoint": "https://api.minimaxi.com",
  "model": "speech-2.8-turbo",
  "voice_id": "female-tianmei",     // 甜美温柔女声；想要成熟一点可换 female-yujie / female-chengshu
  "api_key_env": "MINIMAX_API_KEY", // key 优先从这个环境变量读
  "proxy": "http://127.0.0.1:7890", // 走代理才通，留空直连
  "sapi_voice": ""                  // 回退 SAPI 时指定语音名，留空自动挑中文女声
}
```

音色定位是「阳光温柔女声」，所以云端合成带 `emotion: "happy"`；回退本地 SAPI 时按
「中文女声 → 任意中文 → 任意女声」挑音色，**一个女声都没有就静默跳过**——宁可不出声，
也不要冒出 `Microsoft David` 那种男声。

**key 不放配置文件**：默认读环境变量 `MINIMAX_API_KEY`（`setx MINIMAX_API_KEY sk-cp-...` 之后重开桌宠）。
`tts.api_key` 只是环境变量拿不到时的兜底字段，正常情况下留空；key 通过 `PET_TTS_KEY`
子进程环境变量传给 PowerShell，不会出现在命令行/进程列表里。

关于 MiniMax 接口本身实测过的坑：

- 域名用 `https://api.minimaxi.com`（国际站）。`api.minimax.cn` SSL 握手超时。
- 必须用默认的 hex 音频返回。`output_format:"url"` 返回的阿里云 OSS 链接握手超时，拿不到文件。
- 接口是 `POST /v1/t2a_v2`，`Authorization: Bearer <key>`，成功判据是 `base_resp.status_code == 0`。
- 延迟波动很大：稳定时 2–5 秒，偶尔整个请求挂死。脚本默认单次超时 40 秒、失败重试 3 次，
  都失败才退回 SAPI；读 24 字中文约产出 175 KB WAV。
- hex 解码用 `SoapHexBinary`（3 MB 约 0.5 秒），失败才退回逐字节循环。

回退到本地 SAPI 时**语音包要自己装**：Windows 默认只有英文语音（David / Zira），中文文本会被
英文引擎直接吞掉——实测 13 个中文字符输出 46 字节 WAV（纯文件头，零音频）。装中文语音的路径：
设置 → 时间和语言 → 语音 → 添加语音（中文简体）。装完 `Add-Type -AssemblyName System.Speech`
就能看到 `Microsoft Huihui Desktop` 之类的 zh 语音，代码会自动选中。

实现细节：TTS 走 `powershell -File`（不是 `-Command`——PowerShell 5.1 会把长 `-Command`
字符串截断，导致 `Speak` 拿到空值并报 `0x20 音频设备错误`），文本经 UTF-8 临时文件传递，
避开管道编码与引号问题；脚本自身保持纯 ASCII，避免无 BOM 的 UTF-8 `.ps1` 被按 ANSI 解析。
合成/播放失败会把原因写到 `%TEMP%\codex-pet-tts.err`。

## 设置窗口

右键「设置…」（或 `POST /settings`）打开一个独立的图形设置窗口，不用再拿记事本改 `pet.json`。
分四组：声音（TTS 开关/引擎/音色/模型/接口/代理/key 环境变量 + 试听）、说话（听写服务/ffmpeg/语言/麦克风）、
对话（命令每行一个参数、工作目录、环境变量）、其他（端口/大小/总在最前/配置路径）。

- **保存**会校验后整份重写 `pet.json`（先写 `.json.tmp` 再 rename），保留文件里未知的键；
  `tts.api_key` 不会回显到界面，保存时沿用磁盘上的原值。
- **恢复默认**只回填声音/说话/端口/大小/置顶，**保留对话命令、工作目录和环境变量**——
  否则会把本机 Codex 的绝对路径和代理环境清掉。
- 语音开关、缩放、置顶保存后立即生效；端口和对话命令要重启桌宠才生效。

## 对话

点宠物 → 底部出现输入框 → 回车发送。桌宠按 `pet.json` 里的命令模板执行一条 agent 命令，
把 stdout 里能认出的文本逐行显示成气泡（流式追加），跑完再补一次 `done`。

`pet.json` 查找顺序：`PET_CONFIG` 环境变量 → 可执行文件同目录 → `%APPDATA%\codex-pet\pet.json`。
默认内容（`pet.json` 已放在仓库根目录和 `%APPDATA%\codex-pet\`）：

```json
{
  "port": 8765,
  "chat": {
    "command": ["C:\\Users\\yan\\AppData\\Local\\OpenAI\\Codex\\bin\\8925276705115dea\\codex.exe",
                "exec", "--json", "--skip-git-repo-check", "{prompt}"],
    "cwd": "d:\\POC\\chatbot"
  }
}
```

`{prompt}` 会被替换成你输入的文字。**换成任何 agent 都只是改这个数组**，例如：

| agent | command |
|---|---|
| Claude Code | `["claude", "-p", "{prompt}"]` |
| 自定义脚本 | `["python", "d:\\bot\\ask.py", "{prompt}"]` |
| 换模型/沙箱 | 同上，改 `cwd` 与命令参数 |

文本抽取规则：`codex exec --json` 的 `item.completed / agent_message.text`；其它 JSON 里
`text` / `message` / `content` / `delta`；**不是 JSON 的行按纯文本**——所以任何往 stdout
打印答案的 CLI 都能直接接。

Codex 这类“每次起一个进程”的 agent，一问一答实测 38–64 秒；想更快就换成常驻型的 agent 或本地小模型。

## MCP 接入（推荐）

同一个 exe 加 `--mcp` 就是标准 MCP server（stdio JSON-RPC）：不建窗口、不占端口。
在 agent 里注册一次，之后它自己会把桌宠当工具调用。

Codex：

```powershell
codex mcp add codex-pet -- "d:\POC\chatbot\work\codex-pet\src-tauri\target\release\codex-pet.exe" --mcp
```

Codex 默认不放行 MCP 工具调用，会报
`MCP tool call requires approval, but approval policy is never`。在 `~/.codex/config.toml`
里给这张表补一行（`codex mcp add` 不会写这个键）：

```toml
[mcp_servers.codex-pet]
command = 'd:\POC\chatbot\work\codex-pet\src-tauri\target\release\codex-pet.exe'
args = ["--mcp"]
default_tools_approval_mode = "approve"
```

Claude Code：

```powershell
claude mcp add codex-pet -- "d:\POC\chatbot\work\codex-pet\src-tauri\target\release\codex-pet.exe" --mcp
```

暴露四个工具：

| 工具 | 作用 |
|---|---|
| `pet_say` | 弹气泡对用户说一句话（适合完成提示、需要确认、出错提醒） |
| `pet_gesture` | 切姿态 `idle/sleeping/thinking/writing/tool/done/error`，可带一行 `detail`；也能只发一次性动作 `gesture`：`nod/shake/lean/cheer/sigh/drowse/startle` |
| `pet_state` | 读当前姿态与最近一次事件 |
| `pet_speak` | 让它把这句话念出来（MiniMax 云端 TTS，失败回退 SAPI），与 `pet_say` 的区别是只出声 |

工具调用会转发给**正在运行的桌宠**，端口从 `%APPDATA%\codex-pet\endpoint.json` 读——
端口冲突自动顺延（8765→8774），重开桌宠也不用改任何 agent 配置。

### 各家 agent 注册对照表

同一个 exe、同一份 `{"command": "<exe>", "args": ["--mcp"]}`，只是各家的配置文件放法不同。
现成模板在 `clients/`（除 Codex App 用 `plugin/` 的插件三件套）。

| agent | 放哪儿 | 状态 | 审批粒度 |
|---|---|---|---|
| **Codex App** | `~/.codex/config.toml` 加 `[marketplaces.codex-pet]` + `[plugins."codex-pet@codex-pet"]`，指向 `plugin/` | 已写，重启 App 生效 | 支持逐工具 |
| **Codex CLI** | `[mcp_servers.codex-pet]` | 已写 | 支持逐工具 |
| **Claude Code** | `~/.claude.json`（`claude mcp add --scope user`） | 已注册 | `permissions.allow` 白名单 |
| **CodeBuddy / WorkBuddy** | 用户级 `~/.codebuddy/.mcp.json`；桌面版走 MCP 面板 | 模板 `clients/codebuddy.mcp.json` | 无 |
| **DSH**（DeepSeek Harness） | Claude 风格 mcp 配置，或装 `dsh-mcp-bridge` | 模板 `clients/dsh.mcp.json` | 无 |
| **Trae** | MCP 面板 → 添加 → 「原始配置(JSON)」粘贴 | 模板 `clients/trae.mcp.json` | 无 |
| **千问办公 QwenWork** | 扩展 → 连接器 → + 添加 → 粘贴 JSON / 手动 STDIO | 模板 `clients/qwenwork.mcp.json` | 无 |
| **豆包工作** | 插件·技能·伙伴 → 插件 → + 添加 → 新建自定义插件 | 手动填表，无 JSON | 无 |
| **任意脚本 / CI** | `POST 127.0.0.1:<port>/event` 等 5 个 HTTP 端点 | 见下一节 | — |

豆包和千问办公的表单字段一样：**传输类型 STDIO**、服务器名称 `codex-pet`、
运行命令 = `d:\POC\chatbot\work\codex-pet\src-tauri\target\release\codex-pet.exe`、参数 = `--mcp`。

审批策略（目前只有 Codex 和 Claude Code 支持逐工具配）：

- `pet_state` / `pet_gesture` 自动放行 —— Codex 插件里 `default_tools_approval_mode = "approve"`；
  Claude Code 在 `~/.claude/settings.json` 的 `permissions.allow` 里放了
  `mcp__codex-pet__pet_state`、`mcp__codex-pet__pet_gesture`。
- `pet_say` / `pet_speak` 每次确认 —— 插件 `.mcp.json` 里这两个工具单独写了 `approval_mode = "prompt"`。
- 其它客户端没有逐工具开关，默认都是"每次问"。

**关于 env 里的 key**：TTS 是在**已经跑起来的桌宠进程**里合成的，MCP 子进程只负责把请求转发到
`127.0.0.1`，所以 `MINIMAX_API_KEY` 其实不必写进 MCP 配置。模板里留的是
`"MINIMAX_API_KEY": "${MINIMAX_API_KEY}"`（Claude Code 会展开，展开不了就当字面量传，无害）；
真 key 请留在系统环境变量里，别提交进仓库。

## 自定义 agent 接入（本地 HTTP）

桌宠在 `127.0.0.1:8765`（端口被占会自动顺延到 8774，真实端口写在
`%APPDATA%\codex-pet\endpoint.json`）开了 6 个端点，只绑本机：

```powershell
# 推状态（驱动动画）：idle/sleeping/thinking/writing/tool/done/error
curl -X POST http://127.0.0.1:8765/event -d '{"activity":"tool","detail":"Bash"}'

# 直接弹气泡
curl -X POST http://127.0.0.1:8765/say -d '{"text":"构建完成"}'

# 只出声不起气泡
curl -X POST http://127.0.0.1:8765/speak -d '{"text":"build finished"}'

# 让桌宠去问 agent（和点宠物输入框等价）
curl -X POST http://127.0.0.1:8765/chat -d '{"text":"看看这个目录"}'

# 读当前状态
curl http://127.0.0.1:8765/state

# 打开设置窗口
curl -X POST http://127.0.0.1:8765/settings
```

这样 DSH、Claude Code、CI 脚本、甚至 cron 都能驱动同一个宠物：能发事件的发 `POST /event`，
能打印答案的走 `POST /chat` 或改 `chat.command`。

## 结构

```
src-tauri/src/main.rs   tail + 状态机 + 本地 HTTP + chat 子进程 + TTS + 语音输入 + 配置读写
src-tauri/src/mcp.rs    --mcp 模式的 stdio MCP server（转发给运行中的桌宠）
src-tauri/tauri.conf.json 主窗口（宠物，320×380 逻辑像素）+ 设置窗口（540×680）
src-tauri/capabilities/default.json 两个窗口的权限（拖动/隐藏/显示/关闭）
ui/app.js              状态切图集、气泡、输入框、语音输入、右键菜单
ui/style.css           各状态 CSS 动效、气泡/输入框/菜单样式
ui/sprites/*.png       8 张 1024×1024 4×4 雪碧
ui/settings.html/js/css 图形设置界面（保存 / 恢复默认 / 试听）
src-tauri/icons/alice.ico 托盘/窗口图标（alice 头像，16–256 多尺寸）
pet.json               端口、chat 命令模板、语音开关、tts/asr、缩放、置顶
plugin/                Codex App 的本地插件市场（marketplace.json + .codex-plugin + .mcp.json）
clients/*.json         其它 agent 的 MCP 注册模板（Claude/CodeBuddy/DSH/Trae/千问办公）
```

## 已知限制

- Codex 的审批等待**不落盘**，所以没有 `awaiting` 状态（要补得接 app-server）。
- 日志没有 token 级流式事件，`writing` 只能靠 `token_count` 心跳推断，粒度较粗。
- 日志格式是 Codex 内部持久化格式，升级后可能变；解析失败只会少几个中间态，不会崩。
- 多会话并发时只看**最近修改**的那个 rollout 文件。
- `/chat` 不做并发限制，连点两次会起两个 agent 进程。
- 桌宠没运行时 MCP 工具会返回“没找到运行中的桌宠”，不会自动把桌宠拉起来。
- 云端 TTS 需要 `MINIMAX_API_KEY`；拿不到 key 就只剩本地 SAPI，而本机 SAPI 只有英文语音，
  中文文本不出声（见上文「语音」）。
- 语音输入需要本机先跑着 `whisper-server`，桌宠不会自动拉起；服务不通会直接报错。
- 静音时 whisper 的幻听靠黑名单过滤，换模型/语言后可能漏网，需要补 `is_asr_hallucination` 的条目。

## 后续升级（可选）

接 `codex app-server` 的 JSON-RPC 协议可补齐流式与审批事件，那套 schema 已生成在 `work/codex-appserver-schema/`。
