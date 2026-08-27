# AGENTS.md

mini-mdr 的 AI 编码代理约定。改动前请先读此文件。

## 提交前必做（防止 CI 失败）
- **每次提交 Rust 改动前，必须运行 `cargo fmt --all`，并把格式化结果纳入本次提交。**
  - CI 执行 `cargo fmt --all -- --check`，任何格式不符都会让流水线以 exit 1 失败。
  - `cargo fmt` 只做格式化，**不需要链接器**；本机（无 MSVC `link.exe`）也能正常运行。
  - 不要提交任何未经 `cargo fmt --all` 格式化的 `.rs` 文件。
- 推荐工作流：改完代码 → `cargo fmt --all` → `git add -A` → `git commit` → `git push`。

## 发布纪律
- **不要**自动修改 `Cargo.toml` 版本号、打 tag 或触发 release。
- 只有用户明确说“release一下”时才执行：把 `Cargo.toml` 版本号升到下一位（当前最新 tag 为 `v0.2.13`），提交，打 `vX.Y.Z` tag 并 `git push --tags`，由 CI 自动出 release。
- 平时功能/修复改动直接提交并 `git push origin main`，不打 tag。

## 环境注意
- 本机 Windows 缺 MSVC 链接器，`cargo build` / `cargo check` 会失败；编译由 CI（windows-latest）校验。
- `cargo fmt` 在本机可用，请善用它在提交前自查格式。

## 代码约定
- 历史记录以 `history.json`（config 目录）为唯一真相源；内存中不再缓存 `RendererState.history`。
- SSE：`GET /events` 通过 `notify` 监视 config 目录下 `history.json` 的变更来推送 status / history，不做定时轮询。
- 新增语言：在 `src/i18n.rs` 的 `LANGUAGES` 增加一项，并新增 `locales/<code>/main.ftl`，无需改动设置页代码。
- 设置页 UI 位于 `resources/index.html`（Tailwind CDN，亮色专业风格）；所有用户可见文案经 `{{KEY}}` 模板注入，文案在 `locales/*/main.ftl`。
