# SourceHarness

从零构建的独立 Rust 工具调用测试内核。与 MyBuddy 无关，不依赖 Pi、Electron 或 Node.js。

[第一关开发、测试与验收规范](docs/stage-1-tool-calling.md)

## 当前状态

当前只做第一关，先验模型协议、单工具和结果回填。第一关通过后停下提交证据；不自动进入 Agent Loop。

## 在 omarchy 运行

本项目使用已安装的隔离 Cargo；下面的命令须在仓库根目录运行。真实测试走本机 Magpie 的 `http://127.0.0.1:3425/v1/chat/completions`，固定模型 `opencode-go/deepseek-v4.1-flash`，不走自动路由。

```sh
CARGO="$HOME/.local/share/sourceharness/toolchain/bin/cargo"
"$CARGO" test --locked
"$CARGO" run --locked -- smoke-model
"$CARGO" run --locked -- smoke-declare
"$CARGO" run --locked -- smoke-execute
"$CARGO" run --locked -- roundtrip 3
"$CARGO" run --locked -- roundtrip 100
```

最后一项每个样本最多两次真实模型请求。只发送合成数据，不自动重试；通过结果回填的冒烟测试不代表第一关验收通过。

真实模型证据保存在本地 `evidence/`，不提交密钥、原始鉴权头或未经脱敏的日志。Linux 开发不代替 Windows 实机验收。
