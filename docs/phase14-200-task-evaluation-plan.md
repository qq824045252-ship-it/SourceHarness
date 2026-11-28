# SourceHarness Phase 1–4 二百题评测与闭环优化方案

> 目标分支：`v2-clean`（Phase 1–4 落地提交 `8697d60`）
> 前置基线：100 题实时基准（`bench-100-c3-final`，94/100 通过）
> 目的：在不回退单 Agent 能力的前提下，量化验证多文件原子修改、L1 并发、时间线分叉与 L2 合流安检门，并把实测轨迹转成下一轮优化切片。

---

## 一、 200 题测试矩阵（双轨 100 + 100）

现有 [`qwen_benchmark_cases.py`](../scripts/qwen_benchmark_cases.py) 提供 50 种题型 × 20 变体（共 1,000 题），此前只跑过 `variant 00–01`。200 题拆为两条互补轨道。

### 轨道 A：100 题标准泛化回归集（`variant 02–03`）

- 复用 50 种题型 × 2 个**未见过**的种子，覆盖 `inspect (20)`、`patch (20)`、`report (16)`、`repair (20)`、`workflow (14)`、`guard (10)`，并保留 1K / 4K / 16K 文件体积梯度。
- 检验目标：
  1. Phase 1–4 引入的新代码（多文件 `edit(files=[...])`、L1 并发调度、`blobs/` 回退兼容）对单 Agent 能力**零回归**；
  2. Cycle 1–3 的优化不是对 `variant 00–01` 过拟合，在未见数据上维持高通过率、低 Token 与低 P95。
- 运行入口：`scripts/eval_qwen_benchmark.py --variant-range 2-3`。

### 轨道 B：100 题 Phase 1–4 专项压力集

新建 `scripts/phase14_benchmark_cases.py` + `scripts/phase14_benchmark.py`，4 大类 × 25 题：

| 专项类别 | 题量 | 覆盖场景（含 1K / 4K / 16K 梯度） | 核心验收点 |
|---|---:|---|---|
| **1. 多文件原子修改 & 任意节点回溯** | 25 | ① 3–5 文件跨模块联动重构（`atomic-rename` 10 题）<br>② 第 4 个文件锚点缺失触发预检失败（`atomic-abort` 8 题）<br>③ 错误变更集 → `rollback` → 正确变更集（`atomic-rollback` 7 题） | 预检失败时已通过的前置文件 **0 字节落盘**；一次 `edit(files=[...])` 只产生一个 `tx_id`；`rollback` 整组撤销且 `checkpoint_event` 指向 run 起点。 |
| **2. 单轮多工具并发 & 写屏障** | 25 | ① 4 个外部检查器单轮扇出（`validate-fanout` 13 题）<br>② 单轮混合 `read + validate + edit + read`（`write-barrier` 12 题） | 并行 `validate` 墙钟耗时显著低于串行之和；`call_id` 顺序 100% 确定；静态前缀一致对 100%。 |
| **3. 死胡同分叉 & 上下文回溯** | 25 | ① 死胡同后 `branch --event 1` 重来（`branch-rewind` 10 题）<br>② 从最后一个绿灯 `validation_receipt` 分叉（`branch-green` 5 题）<br>③ 同点双分支 A/B 择优（`branch-ab` 10 题） | 分叉后 `0..=K` 事件前缀**字节级 100% 一致**；工作区精准恢复到 `K`；分叉后首个请求与主线同位置请求**字节相同**（KV-cache 命中）。 |
| **4. 多 Worker 隔离并发 & 安检门合流** | 25 | ① 不重叠模块并发（`worker-disjoint` 8 题）<br>② 同文件不同区段三路合并（`worker-shared-file` 5 题）<br>③ Write Skew 语义冲突拦截后二次合流（`worker-write-skew` 8 题）<br>④ 同行硬冲突拦截后重开 Worker 合流（`worker-conflict` 4 题） | 冲突或语义校验失败时主线 **100% 零污染**；`MERGE_CONFLICT` / `MERGE_VALIDATION_FAILED` 可复现；修复后二次合流成功。 |

轨道 B 的每个 case 是一段**确定性动作脚本**（agent 运行、`branch`、`worker spawn`、`worker merge`）＋独立评分器，因此同一份语料有两种驱动方式：

- `--driver scripted`（默认）：本地 SSE 网关按脚本回放工具调用，**离线、零 API 成本、可进 CI**，用于验证机制不变量；
- `--driver live`：接真实模型，测**采用率**（模型是否自发使用 `edit(files=[...])`、单轮扇出 `validate`、分叉逃生），机制不变量降级为 advisory。

---

## 二、 核心量化指标与门禁

每轮统一产出结构化 `summary.json`，盯紧 6 组硬指标：

1. **准确率**：轨道 A `>= 94%`（剔除 `guard.must-delete` 契约冲突后 `>= 96%`）；轨道 B `>= 90%`，其中**原子预检零半写率**与**主线零污染率必须 100%**。
2. **轮次与延迟**：总请求数、`requests_p50 / p95`、`seconds_p95`；Phase 2 额外记录并发加速比 `sum(validate elapsed_ms) / wall_ms`（4 检查器目标 `>= 1.8`）。
3. **Token 与缓存**：总 Token、输出 Token、静态前缀一致对（**100%**）、分叉后 `0..=K` 前缀复现率（**100%**）。
4. **工具健康度**：总报错率、`exec` 调用占比、多文件任务中 `edit(files=[...])` 原子批处理采用率、`TEXT_NOT_FOUND` 等预检错误分布。
5. **并发安全率**：Write Skew 语义拦截率（**100%**）、二次修复合流成功率。
6. **崩溃与恢复**：保留 `SIGKILL` 中断恢复 + 14/14 对抗用例 + 全部 Rust 单测/集成测试通过。

---

## 三、 预判瓶颈与重点优化方向（随实测轨迹驱动）

先锁定 5 个最可能在高强度 200 题中暴露的真实优化点（全部遵循 `AGENTS.md`，不加防御性编程）：

1. **多文件原子 `edit(files=[...])` 的模型采用率引导**
   - 现状：`compact.rs` 已支持，但静态 `INSTRUCTIONS` 未提示"修改多个关联文件时优先一次原子提交"。
   - 优化：若轨道 B 第 1 类实测仍拆成多轮单文件 `edit`，在 `INSTRUCTIONS` 补一句精炼说明，观察轮次与半失败率下降幅度。
2. **`worker rebase` 辅助原语**
   - 现状：`merge_worker` 拦截后主线毫发无伤，但 Worker 目录停在旧 `base_revision`，需人工同步主线改动。
   - 优化：`merge.rs` + `main.rs` 增加 `worker rebase --root <main> --worker <dir>`：三路合并主线 head 到 Worker、更新 `worker.json` 的 `base_revision`、冲突时写入标准冲突上下文。
3. **原子变更集中途 `SIGKILL` 的恢复闭环**
   - 现状：`apply_changeset` 预检通过后逐文件落盘，中途被杀只按单条 `edit_intent` 识别。
   - 优化：补齐 `changeset_intent` / `resume_changeset`，在 `tests/session_recovery.rs` 增加注入测试。
4. **L1 并发扩展到纯 `read` 扇出**
   - 现状：仅当批次内含 `validate` 才走 `std::thread::scope`。
   - 优化：若 16K+ 大文件或全仓搜索扇出耗时占比明显，把 `>= 2` 个无副作用 `read` 也纳入并行预计算。
5. **Loop Guard → 自动分叉回溯编排**
   - 现状：`branch` 需调用方手动指定 `--event K --lesson`。
   - 优化：新增检查点定位器（自动找最后一个绿灯 `validation_receipt` 或 `user` 起点），让上层 Runner 在 `LOOP_GUARD_TRIGGERED` 时自动派生分支，验证卡死任务自动逃生率。

---

## 四、 执行节奏（4 步闭环）

1. **Step 1（建题与冒烟）**：`phase14_benchmark_cases.py`（100 题语料）+ `phase14_benchmark.py`（驱动与评分器）+ `phase14_checker.py`（声明式外部检查器）；扩展 `eval_qwen_benchmark.py` 支持 `--variant-range 2-3`；先跑离线单测与 8 题冒烟，确认评测器自身 0 Bug。
2. **Step 2（200 题基线）**：`v2-clean` 上跑完整 200 题，沉淀基线 `summary.json` 与失败聚类报告。
3. **Step 3（切片循环优化）**：挑出失败与高耗时样本（15–20 题切片），按上述 5 个方向逐项改代码 → 全量离线校验（`cargo test` / `clippy` / 对抗 / Python 单测）→ 切片复测验证收益，无回归才合并。
4. **Step 4（终测与归档）**：跑完最终 200 题，输出前后量化对比表，更新文档并提交推送。

---

## 五、 Step 1 交付物与用法

| 文件 | 作用 |
|---|---|
| `scripts/phase14_benchmark_cases.py` | 100 题确定性语料（4 类 × 25），含工作区、检查器声明、动作脚本与期望 |
| `scripts/phase14_benchmark.py` | 驱动（agent / branch / spawn / merge）+ 6 组指标评分 + `summary.json` |
| `scripts/phase14_checker.py` | 声明式外部检查器（`lint` / `markers` / `signature` / `exact` / `absent`） |
| `scripts/test_phase14_benchmark.py` | 语料确定性与检查器单测 + 8 题离线冒烟 |

```bash
# 语料完整性（不跑模型）
python scripts/phase14_benchmark.py --plan-only

# 8 题离线冒烟（脚本化网关，零 API 成本）
python scripts/phase14_benchmark.py --exe target/debug/source_harness --smoke --out results/phase14-smoke

# 100 题专项全量（脚本化，验证机制不变量）
python scripts/phase14_benchmark.py --exe target/debug/source_harness --out results/phase14-scripted

# 100 题专项全量（真实模型，测采用率）
python scripts/phase14_benchmark.py --exe target/debug/source_harness --driver live \
  --base-url https://opencode.ai/inference/go/openai/v1 --out results/phase14-live

# 轨道 A：100 题未见过变体
python scripts/eval_qwen_benchmark.py --exe target/debug/source_harness --variant-range 2-3 \
  --out results/bench-100-trackA
```

---

## 六、 Step 1 实测结果（脚本化驱动，`v2-clean`）

`--driver scripted` 用本地 SSE 网关按脚本回放工具调用，因此测的是**机制正确性**而非模型能力；轨道 B 全部 100 题一次通过，说明评测器与四类机制契约自洽。

| 指标 | 实测值 |
|---|---:|
| 评测 / 通过 | 100 / **100（100%）** |
| 动作数（agent / branch / spawn / merge） | 273 |
| 请求数（`p50 / p95`） | 522（`4 / 9`） |
| `seconds_p95` | 2.768s |
| 合流安检门 | `ok 25` / `MERGE_VALIDATION_FAILED 8` / `MERGE_CONFLICT 4` |
| `validate` 并发加速比（`p50 / max`） | **2.65 / 2.85**（4 检查器 × 350ms） |
| 静态前缀一致对 | **218 / 218（100%）** |
| 分叉前缀复现 | 35 个分支全部 `0..=K` 字节一致；35 个分叉后首请求前缀全部命中 |
| 工具分布 | `edit 192`、`validate 165`、`read 48`、`rollback 7` |
| 失败原因 | 无 |

分类结果：`atomic 25/25`、`parallel 25/25`、`branch 25/25`、`worker 25/25`。

关键断言均已落地为硬性检查：原子预检失败后工作区**字节零变化**（`atomic-abort` 8 题）、一次 `edit(files=[...])` 只产生一个 `tx_id` 且无游离 `edit`（`atomic-rename` 10 题）、`rollback` 的 `checkpoint_event` 指向 run 起点并整组撤销 4 个文件（`atomic-rollback` 7 题）、合流被拒后主线**字节零污染**（`worker-write-skew` 8 题 + `worker-conflict` 4 题）。

尚未执行：Step 2 的 200 题**实时**基线（轨道 A `--variant-range 2-3` 与轨道 B `--driver live`）需要模型凭据与约 1–2 小时机时，留待 Step 2 执行。


---

## 七、 200 题实时基线（`auto/deepseek-v4.1-flash-1017`，本机 magpie 网关）

> 注：本节记录 2026-11-28 的早期基线（`b8ee49e`，worker rebase 与题干澄清之前）。定版数字见第八节。

2026-11-28 实跑，两条轨道合计 200 题，全部真实模型调用。逐题数据与三份 `summary.json` 已归档在 [`docs/evidence/phase14-200-2026-11-28/`](evidence/phase14-200-2026-11-28/README.md)。

### 轨道 A：100 题泛化回归（`variant 02–03`）

| 指标 | 实测 |
|---|---:|
| 评测 / 通过 | 100 / **95（95.0%）** |
| 请求数（`p50 / p95`） | 612（`6 / 10`） |
| `seconds_p95` | 15.98s |
| 总 Token | 1,373,952（input 1,305,060 / output 68,892 / reasoning 23,901） |
| 缓存命中 | 1,150,976（**88.2%**） |
| 静态前缀一致对 | **512 / 512（100%）** |
| Loop Guard 暂停 | 0 |
| 工具分布 | `read 274`、`exec 117`、`validate 99`、`edit 43`、`write 31`、`rollback 6` |

失败 5 题，全部是已知题型：

- `guard.must-delete.02/.03`：评测契约假设无删文件能力，但 `exec` 能删，模型删完按成功回复（与 `variant 00–01` 完全同源）。
- `inspect.paged-window.03`、`inspect.unique-times.03`：最终答案与期望不符。
- `workflow.history-audit.03`：旧快照引用不可验证。

**结论**：与 `variant 00–01` 的 94/100 基本持平（+1 题），Cycle 1–3 的优化没有对旧种子过拟合。

### 轨道 B：100 题专项（真实模型）

| 指标 | 实测 |
|---|---:|
| 评测 / 通过 | 100 / **77（77.0%）** |
| 动作数 | 273 |
| 请求数（`p50 / p95`） | 1,157（`10 / 22`） |
| `seconds_p95` | 37.19s |
| 总 Token | 2,523,190（input 2,421,391 / output 101,799） |
| 静态前缀一致对 | **584 / 584（100%）** |
| 合流安检门 | `ok 16` / `MERGE_VALIDATION_FAILED 16` / `MERGE_CONFLICT 5` |
| 工具分布 | `read 606`、`edit 226`、`validate 207`、`exec 156`、`write 6`、`rollback 2` |

分类结果与归因：

| shape | 通过 | 归因 |
|---|---:|---|
| `atomic-rename` / `atomic-abort` / `atomic-rollback` | 10/10 · 8/8 · 7/7 | 原子变更集、预检失败零半写、整组回滚**全部成立**；模型会先读文件再改锚点，`atomic-abort` 因此改为"零半成品状态"双接受态 |
| `validate-fanout` | 13/13 | 单轮 4 检查器扇出稳定；加速比 `p50 1.63`（含 2 检查器样本） |
| `branch-rewind` / `branch-green` / `branch-ab` | 10/10 · 5/5 · 9/10 | 分叉 `0..=K` 字节一致 + 分叉后首请求前缀命中**全部成立**；1 例为 oracle 格式过严 |
| `worker-disjoint` / `worker-shared-file` | 8/8 · 5/5 | 隔离并发与三路合并正常 |
| `write-barrier` | **0/12** | 12/12 改用 `exec` 跑 mypy/ruff，仅 2/12 调用配置的 `validate`，并在工作区留下 `.mypy_cache/`、`.ruff_cache/` |
| `worker-write-skew` | **0/8** | 首次合流按预期被 `MERGE_VALIDATION_FAILED` 拦截且主线零污染，但 Worker 停在旧 `base_revision`，看不到主线新签名，二次修复必然失败 |
| `worker-conflict` | 2/4 | 1 例 CLI 异常退出；1 例主侧改写文本导致两侧不再同行冲突 |

### 三个可直接进 Step 3 的结论

1. **`worker rebase` 是刚需，不是优化项**（瓶颈 #2 得到 8/8 实测确认）。合流被拒后 Worker 无法获知主线签名变更，模型只能"看不到问题就什么都不改"，二次合流必然失败。修复方向：`merge.rs` + `main.rs` 增加 `worker rebase --root <main> --worker <dir>`，三路合并主线 head 到 Worker 并更新 `worker.json` 的 `base_revision`。
2. **`validate` 的调用优先级需要显式指令**（12/12 实测）。检查名 `lint`/`types` 会让模型理解为"自己去跑 mypy/ruff"，从而绕过配置好的检查器并污染工作区。修复方向：在静态 `INSTRUCTIONS` 里明确"仓库检查一律用 `validate`，`exec` 只用于临时验证且不得留下缓存目录"；或在 `validate` 描述里列出已配置的检查名。
3. **oracle 要按"语义等价"判分**（3 例）。模型会补 docstring、改写措辞、调整格式；字节级 exact 会把正确结果判负。修复方向：把 `expect` 的 exact 换成 markers/`any_of`，只在脚本化机制题保留字节比对。

---

## 八、 定版（`b8bc33e`，2026-11-28）

三条轨道全部用同一 commit、同一二进制跑完（`cargo build --locked`，`sha256 = 7aae8231490267c03c5ece3fb5aff14fa0a5ba7f0f25572af3638465cc9e4628`），各自使用全新输出目录；逐题数据与三份 `summary.json` 归档在 [`docs/evidence/phase14-200-b8bc33e-2026-11-28/`](evidence/phase14-200-b8bc33e-2026-11-28/README.md)，每份 `summary-*.json` 都由同目录对应的 `cases-*.jsonl.gz` 生成，配对与摘要见该目录 `provenance.json`。

| 配置 | 通过 | 早期基线（`b8ee49e`） |
| --- | ---: | ---: |
| 轨道 A · 100 题泛化（live） | **96/100** | 95/100 |
| 轨道 B · 100 题专项（live） | **95/100** | 77/100 |
| 轨道 B · 100 题专项（scripted） | **100/100** | 100/100 |

关键指标：轨道 A 613 请求（`p50/p95 6/11`）、`seconds_p95 19.40s`、1,372,651 token（缓存 88.5%）、前缀 **513/513**；轨道 B live 1,155 请求（`10/22`）、`seconds_p95 25.62s`、2,387,988 token、前缀 **584/584**、合流安检门 `ok 24` / `VALIDATION_FAILED 10` / `CONFLICT 3`；轨道 B scripted 522 请求、前缀 **218/218**。

机制真实触发率（`triggered / evaluated`）：

| 机制 | Track B live | Track B scripted |
| --- | ---: | ---: |
| 原子多文件变更集 | 25/25 | 17/25 |
| 单轮并行 `validate` | 16/25 | 25/25 |
| 显式 `rollback` 回滚 | 1/7 | 7/7 |
| Worker `rebase` 后二次合流 | `ok 8` | `ok 8` |

相对早期基线的提升可逐项归因：`c7f41bd`（worker rebase）+ `7927734`（评测计入 rebase）把 `worker-write-skew` 从 0/8 抬到 8/8、worker 家族 15/25 → 23/25；`10b016f`（写屏障题干点名已配置的 `lint`/`types`）把 `write-barrier` 从 0/12 抬到 11/12；`7242ca1`（写偏斜题干写明 `2x`、`scale=2` 即 `4x`、默认 `1`）消除了 `x * scale` 与 docstring 两种合法解读。`b8bc33e` 的变更集前滚恢复没有 benchmark case 依赖崩溃，作用在恢复正确性上。

方案 B（在当轮 user 消息尾部注入检查名）在 write-skew 上实测 **0/7**、637,831 token、91 次 `validate`、5 次 `REBASE_CONFLICT`，已回退且代码未保留；修复因此落在题干而不是内核。

轨道 B live 剩余 5 例失败全部来自 oracle 严格度而非机制：`write-barrier.07`、`validate-fanout.00` 的 `app/service.py` 语义正确但字节不同；`atomic-rollback.06` 未通过该题的 `acceptance` 检查；`worker-conflict.02/.03` 改写 `notes.md` 措辞导致 marker 缺失（`.03` 还因主线改写使同行冲突消失）。下一步按第七节结论 3 把这类断言换成 markers / `any_of`，脚本化机制题保留字节比对。
