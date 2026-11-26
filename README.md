# SourceHarness

从零构建的独立 Rust 工具调用测试内核。与 MyBuddy 无关，不依赖 Pi、Electron 或 Node.js。

[第一关开发、测试与验收规范](docs/stage-1-tool-calling.md)

## 当前状态

仓库和 CLI 骨架已初始化，所有验收项均为 **NOT_RUN**。先从 M01–M04（真实模型连通与协议解析）开始，选定一个支持原生工具调用的基线接口后，按文档逐项验证。第一关通过后停下提交证据；不自动进入 Agent Loop。

```sh
cargo run -- --help
cargo test
```

真实模型证据保存在本地 `evidence/`，不提交密钥、原始鉴权头或未经脱敏的日志。Linux 开发不代替 Windows 实机验收。
