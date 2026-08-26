# mini-mdr

[English](README.md)

`mini-mdr` 是一个仅显示系统托盘图标的跨平台 UPnP/DLNA 数字媒体渲染器
(DMR)。它接收来自手机或桌面端控制点 (DMC) 的播放指令，并通过可替换的播放器
后端执行播放。

默认播放器后端是外部 [`mpv`](https://mpv.io/) 进程，通过 mpv 的 JSON IPC
接口控制。

## 功能特性

- 支持 Windows、macOS 和 Linux
- 音频与视频播放 URL
- UPnP/DLNA MediaRenderer 设备描述
- SSDP 发现（响应 `M-SEARCH`、周期性发送 `ssdp:alive`、停止时发送 `ssdp:byebye`）
- `ConnectionManager`：协议信息与连接查询
- `AVTransport`：`SetAVTransportURI`、`Play`、`Pause`、`Stop`、`Seek`
  （`REL_TIME`）、`GetTransportInfo`、`GetPositionInfo`、`GetMediaInfo`
- `RenderingControl`：主声道音量与静音控制
- GENA 事件订阅，支持续订、过期和 `LastChange` 通知
- 可插拔的 `PlayerBackend` 抽象
- 默认 `mpv` 后端，通过 JSON IPC 通信（Windows 上使用命名管道，其他平台使用
  Unix 套接字）；mpv 进程在首次加载媒体时懒启动
- 仅托盘应用控制，包含动态的开始/停止 Cast 标签
- 懒启动的本地设置服务器，带可编辑表单，持久化保存配置
- 媒体历史记录跟踪，可配置最大条目数（持久化到磁盘）
- 信号处理，支持干净关闭（Unix 上的 SIGINT、SIGHUP、SIGTERM）
- 启动时自动开始 Cast
- 内嵌 32x32 RGBA8 托盘图标，启动时无需图片解码
- 国际化支持：中文和英文，根据系统语言环境自动检测

托盘菜单包含三个选项：

1. `开始 Cast` / `停止 Cast`（标签反映当前 Cast 状态）
2. `打开设置`
3. `退出程序`

## 架构

```text
DMC: 手机 / 桌面端控制器
    │ SSDP, SOAP, GENA
    ▼
mini-mdr
    ├── SSDP 发现
    ├── UPnP HTTP 和 SOAP 服务
    ├── 渲染器状态
    ├── 播放器后端抽象
    └── 系统托盘
            │
            ▼
        mpv JSON IPC
            │
            ├── 视频输出
            └── 音频输出
```

DMC 向 `mini-mdr` 发送控制命令。DMR 将媒体 URL 传递给播放器后端。由播放器
（而非 DMC）通过 HTTP 从 DMS 获取实际媒体内容。

```text
DMC ── SOAP 控制 ──> mini-mdr ── IPC ──> mpv
DMS ── HTTP 媒体 ───────────────────────> mpv
```

## 环境要求

- Rust 稳定版工具链
- 带有系统托盘的桌面环境
- `mpv` 已安装并位于 `PATH` 中（除非配置了自定义路径）
- 局域网内的 UPnP/DLNA 控制点

Windows 上请使用 MSVC Rust 工具链。Linux 桌面环境必须提供兼容托盘的通知区域
或 StatusNotifier Host。无头环境可以运行非托盘的协议组件，但托盘创建预计会
优雅地失败。

## 运行

安装 `mpv`，然后执行：

```text
cargo run
```

应用启动时自动开始 Cast 服务。设置 HTTP 服务器是懒启动的，只有在首次点击
`打开设置` 时才会启动。

## 运行行为

### 开始 / 停止 Cast

`开始 Cast` 启动 UPnP HTTP 服务器和 SSDP 发现服务，同时创建已配置的播放器
后端。启动后，兼容的 DMC 客户端可以发现并控制渲染器。

`停止 Cast` 停止当前播放，释放播放器后端，并停止 Cast 服务。如果设置服务器
已经启动，不会被停止。

### 打开设置

设置服务器是刻意懒启动的：

```text
应用启动           -> 不监听设置端口
首次点击 打开设置   -> 绑定 127.0.0.1:7878
端口冲突           -> 选择临时端口作为备选
后续点击           -> 复用同一服务器
```

服务器仅监听本地回环地址，并使用系统默认浏览器打开页面。当前设置包括设备
名称、播放器后端和 mpv 路径。

### 退出程序

`退出程序` 停止 Cast 服务、停止播放器进程、关闭设置服务器（如果已启动），
然后终止应用。

## 配置

默认配置如下：

```toml
[device]
name = "mini-mdr(<hostname>)"   # hostname 来自 $HOSTNAME 或 $HOST

[player]
backend = "mpv"
mpv_path = "mpv"

[settings]
port = 7878
max_history = 200
```

设备名称默认包含系统主机名（例如 `mini-mdr(laptop)`）。历史记录条目持久化
保存到配置目录下的 `history.json` 文件。

配置从 `directories` crate 提供的平台特定用户配置目录加载。如果配置文件
不存在，则使用上述默认值。

## 播放器后端

协议层依赖 `PlayerBackend` trait，而非直接依赖 mpv：

```text
PlayerBackend
    └── MpvBackend (default)
```

当前接口涵盖加载、播放、暂停、停止、跳转、音量、静音和状态查询。未来的
后端（如 GStreamer 或 FFmpeg）应实现此 trait 并在 `player::create_backend`
中注册。

当前插件模型为编译期可扩展性，通过配置选择 Rust 实现。未实现第三方 DLL 或
共享库的运行时加载。

设备和服务描述以独立文件形式维护在 `resources/` 目录下，在编译时通过
`include_str!` 嵌入可执行文件。因此部署时不需要单独的 XML 文件。

可编辑的托盘图标源文件为 `resources/icon.png`。提交图标更改前，请先离线将其
转换为 `resources/icon.rgba`：32x32、行优先、非预乘 RGBA8（4096 字节）。
可执行文件通过 `include_bytes!` 嵌入原始字节并直接传递给 `ldtray`；无需图片
解码器或启动时转换。

## UPnP 范围

当前实现提供了一个刻意精简的协议表面：

- `device.xml` 加每个服务一个 SCPD 文件，编译时嵌入
- SSDP 发现，支持标准搜索目标和生命周期通告
- SOAP 控制：传输、位置/媒体查询、跳转、音量和静音
- 标准 UPnP 错误响应，带错误码
- GENA `SUBSCRIBE`/`UNSUBSCRIBE`：SID 续订、超时限制、过期清理和
  `LastChange` 通知发送给订阅者回调

当前实现尚未达到完整的 DLNA 认证标准。特别是，请求按顺序处理，曲目元数据
有限（无 DIDLLite），仅存在实例 `0`，且广播的协议信息列表是静态的而非从
活跃播放器后端派生。

## 开发

使用以下命令格式化和验证项目：

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

测试播放前，请确认 mpv 可用：

```text
mpv --version
```

有用的手动测试步骤：

1. 启动应用，确认没有设置端口正在监听。
2. 点击 `打开设置`，确认浏览器在 `127.0.0.1` 打开。
3. 点击 `开始 Cast` 并发送 SSDP `M-SEARCH` 请求。
4. 获取返回的 `device.xml` URL。
5. 使用兼容的 DMC 发送 `SetAVTransportURI` 和 `Play`。
6. 停止 Cast，确认 UPnP 和 SSDP 服务已释放。

## 项目状态

这是一个处于活跃开发阶段的早期 MVP。当前状态：

- 核心 UPnP/DLNA 协议已实现（AVTransport、ConnectionManager、RenderingControl）
- SSDP 发现和生命周期管理正常工作
- mpv 后端通过 JSON IPC 控制
- 系统托盘集成，支持开始/停止 Cast、设置和退出
- 设置页面支持配置编辑和媒体历史记录
- 历史记录持久化到磁盘，可配置最大条目数
- Unix 上的信号处理，支持干净关闭
- Arch Linux 的 AUR 打包

优先后续工作：并发请求处理、持久化每安装的 UPnP UDN、DIDLLite 元数据、
从后端动态获取协议信息以及集成测试。
