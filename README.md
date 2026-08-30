# DSH Desktop

一个原生的 macOS 桌面外壳，包装 **DeepSeek Harness**（`dsh web`），让你不用开浏览器标签、以一个真正的桌面应用的方式来使用它。

> **免责声明 · Disclaimer**
> 这是一个**非官方**的社区桌面客户端，与 DeepSeek（深度求索）公司**无任何关联或背书**。`DeepSeek` 名称与 Logo 均为其各自所有者的商标，在本项目中仅用于识别其所包装的产品，本项目不声明其所有权。本应用是对 **DeepSeek Harness**（`dsh web`）做拉起/接管的外壳，引擎本体与全部数据仍由官方 `dsh` CLI 提供。
>
> *Unofficial community desktop client. Not affiliated with or endorsed by DeepSeek. "DeepSeek" and its logo are trademarks of their respective owner, used here only to identify the product this app wraps.*

> 原理：DSH 的核心就是"一个本地 Node 服务 + 一个已预构建的 SPA"。所以桌面端**没有重写任何 AI 引擎**——它只是一个原生外壳，负责拉起/接管同一个引擎，并把窗口导航过去。你看到的**就是同一个 DeepSeek Harness**，功能 100% 一致。

---

## 它是怎么工作的

```
┌──────────────────────────────────────────────────────┐
│  DSH Desktop (Tauri v2, Rust)                        │
│  ├─ 原生窗口 / 菜单栏 / 系统托盘 / 通知               │
│  ├─ 引擎托管（spawn / attach / stop / restart）       │
│  ├─ 单实例锁 · 关闭缩到托盘 · 退出时优雅终止引擎        │
│  └─ 控制器启动页（打包内置）                          │
└──────────────┬───────────────────────────────────────┘
               │ 启动时探测 / 导航
┌──────────────▼───────────────────────────────────────┐
│  dsh 引擎（`dsh web`，同一个 $DSH_HOME 数据）          │
│  └─ 本地服务 http://127.0.0.1:<port> → DeepSeek Harness│
└──────────────────────────────────────────────────────┘
```

### 端口策略（避免和你的命令行实例冲突）

| 情况 | 行为 |
|---|---|
| `127.0.0.1:3080` 上已有 DSH 在服务 | **附加模式**：不新起进程，直接把窗口指向它（同一份数据，另一扇窗）。托盘"Stop Engine"不作用于外部进程。 |
| 3080 空闲 | **自主模式**：桌面端在专有端口 **3480** 拉起 `dsh web --no-open --port 3480`（若被占用则顺延找空闲端口）。 |

两种模式共用同一个 `$DSH_HOME`（默认 `~/.dsh`），所以模型设置、会话、凭据都和你平时的使用完全一致。**注意：不要同时手动再开一个 `dsh web` 共享同一份 `~/.dsh`**（同一套数据最好单实例）。

### 桌面体验功能

- ✅ **系统托盘**：显示/隐藏窗口、Open in Browser、Set Workspace、Restart/Stop Engine、Reveal `~/.dsh`、Quit
- ✅ **应用菜单栏**：DSH Desktop / Engine 两个菜单，同样操作
- ✅ **通知**：引擎异常退出时弹出桌面通知
- ✅ **单实例锁**：再启动一次只聚焦已有窗口
- ✅ **窗口关闭 → 缩到托盘**：点是关不掉 App，继续后台运行
- ✅ **优雅关停**：Quit 时对引擎进程组发 SIGTERM（超时再 SIGKILL），不留孤儿进程/端口
- ✅ **工作区选择**：托盘 "Set Workspace Folder…" 换工作目录后原地重启引擎（DSH 里也可以在界面内切换工作区）
- ✅ **图标**：DeepSeek 鲸鱼 logo（自绘，Dock/Finder 圆角形态）
- ✅ **粘贴文件/文件夹路径**：在 Finder 里复制文件或文件夹后，回到聊天输入框直接 `Cmd+V`，粘出来的就是它的**绝对路径**（原生监听 Cmd+V，检测到"文件拷贝"时自动把剪贴板改写成路径文本；支持 `file:///.file/id=` 引用解析）
- ✅ **标准 Edit 菜单**：Undo/Redo/Cut/Copy/Paste/Select All —— 保证 Cmd+A/C/V/X 在 WebView 的输入框里正常工作

---

## 🚚 给别人用（分发）

- **格式**：macOS 分发标准就是 `.dmg`（拖进 Applications）。产物在
  `src-tauri/target/release/bundle/dmg/DSH Desktop_*.dmg`。
- **对方机器要求**：机器上需要已安装 DeepSeek Harness（`npm i -g @deepseek-ai/dsh`，
  需要 Node.js 18+）。本 App **不会自动安装引擎**——启动时找不到 `dsh` 会给出
  明确的安装指引，装好后重开即可。
  - 若想做到"对方什么都不用装"（连 Node 都不要），可以把引擎依赖（约 272 MB）
    + Node 运行时打进 `.app`——这是可选的后续项，体积会到 ~400 MB。
- **⚠️ 签名/Gatekeeper**：目前是 **ad-hoc 签名**。别人机器首次打开会弹
  “无法验证开发者”。两个选择：
  1. 熟人分发：让对方 **右键 → 打开** 一次即可放行；
  2. 无警告分发：需要 Apple Developer 账号（$99/年）对 `.app` 做 Developer ID
     签名 + 公证（`xcrun notarytool`），这是正式对外发布的正路。
- 引擎数据始终存在对方的 `~/.dsh`，与官方 CLI 的行为完全一致。

---

## 构建与运行

前置：Node ≥ 18、Rust stable 带 `aarch64-apple-darwin` 目标、macOS Command Line Tools（`xcode-select --install`）、可用的 `dsh`（`npm i -g @deepseek-ai/dsh`，或由 npx 缓存自动发现）。

```bash
cd dsh-desktop
npm_config_cache=/tmp/dsh-npm-cache npm install     # 本机 ~/.npm 有 root 文件时用独立缓存

npm run tauri dev                                   # 开发运行（热重载）
npm run tauri build                                 # 发布打包 → target/release/bundle/(app|dmg)
npm run tauri build -- --bundles app                # 只要 .app
```

### 开发者/测试环境变量（不碰真实数据）

```bash
DSH_DESKTOP_HOME=/tmp/test-home        # 引擎使用独立的 $DSH_HOME（默认继承父进程环境）
DSH_DESKTOP_NO_ATTACH=1                # 跳过"附加到 3080"，强制自主拉起
DSH_DESKTOP_PORT=3499                  # 指定桌面引擎端口（默认 3480）
```

示例：`DSH_DESKTOP_NO_ATTACH=1 DSH_DESKTOP_HOME=/tmp/dsh-test npm run tauri dev`

---

## 工程结构

```
dsh-desktop/
├── src/                    # 控制器启动页（纯静态，无打包器；window.__TAURI__ 直调 Rust）
│   ├── index.html          # 深色启动画面：状态、错误、Retry、Open in browser、Set workspace
│   ├── main.js             # invoke start_engine / 监听 engine-status 事件
│   └── styles.css
└── src-tauri/
    ├── Cargo.toml          # tauri + tray-icon + macos-private-api + 4 个插件
    ├── capabilities/default.json  # 只给内置控制器页权限；远程 DSH 页永远拿不到 IPC
    ├── tauri.conf.json     # 单主窗口 1280×840；bundle .app/.dmg
    └── src/lib.rs          # 全部逻辑：
        # 引擎发现（PATH / npx 缓存）· 端口探测（原始 TCP GET，零额外 HTTP 依赖）
        # spawn_engine（独立进程组 + SIGTERM 优雅停止）· attach · watchdog
        # 托盘 / 菜单 / 单实例 / 通知 / 设置持久化
```

### 关键设计决策

- **不启用 `dangerousRemoteDomainIpcAccess`**：窗口导航到 DSH 远程页后，该页与桌面 Rust 之间无任何 IPC 能力。DSH SPA 通过自己的 HTTP/WebSocket 与后端通信，功能完整但桌面桥"只出不入"，安全面最小。
- **单一启动方**：引擎启动统一由控制器页 `start_engine` 驱动（首次启动与托盘 Restart 都回弹该页），Rust `action_start` 内部有 `boot_lock` + "已健康则早退"双重保护，杜绝双启动抢端口（EADDRINUSE）。
- **引擎是子进程**（非内嵌）：与 `dsh web` 完全同构；`--no-open` 防止 CLI 自己弹浏览器。
- **`~/.dsh` 数据即唯一真相**：桌面端不复制、不改写任何 DSH 数据格式。

---

## 已知限制

- 同时运行"桌面实例"与"终端 `dsh web`"共享 `~/.dsh` 可能产生会话存储竞争——桌面端启动时会优先附加已有实例来避免；请勿刻意双开。
- 打包的 `.dmg` 是 ad-hoc 签名，仅供本机使用，不能过 Apple 公证。
- 附加模式下无法用托盘关闭外部进程（它不是我们 spawn 的）。
