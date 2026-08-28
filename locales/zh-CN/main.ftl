# Tray menu
tray-cast = 投屏
tray-more = 更多
tray-quit = 退出程序
tray-open-settings = 打开设置
tray-open-log-dir = 打开日志目录

# Settings page
settings-title = mini-mdr 设置
settings-subtitle = 本地 DMR 设置与运行状态
settings-section-settings = 设置
settings-device-name = 设备名称
settings-player-backend = 播放器后端
settings-mpv-path = mpv 可执行文件路径
settings-vlc-path = VLC 可执行文件路径
settings-save = 保存设置
settings-language = 语言
settings-language-system = 跟随系统
settings-saved = 设置已保存。语言立即生效，其他设置需重启生效。
settings-section-history = 历史记录
settings-history-index = #
settings-history-time = 时间
settings-history-title = 标题
settings-history-copy-link = 复制链接
settings-history-open-in-browser = 浏览器播放
settings-history-copied = 链接已复制
settings-history-empty = 暂无历史记录
settings-history-empty-sub = 您的播放历史将在这里显示。
settings-history-subtitle = 查看最近播放的媒体历史。
settings-max-history = 最大历史记录数
settings-status = 状态
settings-hint = 需重启生效

# Tabs
tab-settings = 设置
tab-history = 历史
tab-about = 关于
tab-guide = 教程

# About page
about-title = 关于
about-subtitle = 了解更多关于 mini-mdr 的信息。
about-description = mini-mdr 是一个仅驻留系统托盘的 UPnP/DLNA 媒体渲染器（DMR）。它会在局域网内广播 DMR，并通过 mpv 或 VLC 播放媒体。

# Guide page
guide-title = 教程
guide-intro = mini-mdr 是一个仅驻留系统托盘的 UPnP/DLNA 媒体渲染器（DMR）。它本身不负责音视频解码，需要依赖外部播放器后端。
guide-player-heading = 1. 安装播放器后端
guide-player-text = 请在系统中安装 mpv 或 VLC 播放器，然后在"设置"中选择播放器后端并填写可执行文件路径。mini-mdr 会通过播放器播放由 DLNA/UPnP 控制器（例如手机 App）推送的媒体。
guide-config-heading = 2. 数据存放位置
guide-config-text = 配置与播放历史保存在：
guide-usage-heading = 3. 使用方法
guide-usage-text = 启动后，mini-mdr 会在局域网内广播一个 DMR 设备。使用任意 DLNA/UPnP 控制器即可投屏到"mini-mdr"。可在托盘菜单切换 Cast 开关，并在设置中修改设备名、语言与播放器。

# Validation errors
error-save-failed = 保存失败
error-name-length = 设备名称必须为 1 到 128 个字符
error-name-control = 设备名称不能包含控制字符
error-unsupported-backend = 不支持的播放器后端
error-player-path-empty = 播放器可执行文件路径不能为空
error-language-invalid = 未知的语言选择

# State display
cast-stopped = 已停止
cast-running = 运行中
transport-no-media = 无媒体
transport-stopped = 已停止
transport-playing = 播放中
transport-paused = 已暂停
transport-loading = 加载中
