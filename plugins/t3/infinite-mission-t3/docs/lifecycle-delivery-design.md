# Lifecycle Delivery Design — IM 恒投两块，T3 按会话生命周期拼装

状态：**设计定稿待实现**（2026-09-15 v2，裁决来自用户；实现前不再改桥侧投递逻辑）。

## 1. 背景与证据

- T3 三层模型：**thread**（`im-<成员>-<mission>`，一 mission 一条，持久）→ **T3 session**（每轮工作完成 settled，下一条消息重拉起；`provider_session_runtime` 的 `stopped` 是拉起前记账，不是会话死亡）→ **provider 会话**（拉起时按 cursor resume 同一条 provider 线程，历史随行）。
- 实证（2026-09-15）：codex 全天单一 rollout 11.8MB 连续追加，内含 18 次旧格式 + 11 次新格式 persona 注入；zcode 单 rollout 文件含 19-26 个轮次标记。**所有 provider 均跨轮恢复，每轮 agent 带完整对话历史开工。**
- 后果：桥的 `sessionStatus stopped/error ⇒ 全量快照` 门与 T3 的 session-bootstrap persona 注入，都在向一条不断增长的 rollout 里**每轮堆一份全量拷贝**（persona 已堆 29 份）。`stopped` 状态在投递时点无法区分「settled 待重拉（历史会回来）」与「真丢历史」——这个信息只有 T3 在会话生命周期内部知道。

## 2. 裁决（用户，2026-09-15，v2）

1. **新会话（provider 线程无历史）→ 全量消息**：persona + 稳定块（meta/Objective）+ 头部 + round 块 + 尾部。
2. **resume（有历史续跑）→ 头部（全量保留）+ round 块 + 尾部**——简要身份行与全量头部差不了多少，不做区分，头部恒投。
3. **persona 归属 T3**（settings 来源不变），**只在新会话发一次**——门从「session bootstrap」（每次重拉都触发）收紧为「provider 线程真无历史」。
4. **工作区不进文案**：T3 在对应工作区生成会话，cwd 已携带该信息。
5. **meta 行的 missionId/origin work/member stations/parent 裁剪为 fresh-only**（每轮完全相同）；**revision 并入站行**随 round 恒投。

职责缝不变：**裁什么归 IM**（块的内容与格式全在桥侧定义），**何时裁归 T3**（按它自己的会话生命周期事实选择块）。T3 对 IM 格式零理解。

## 3. 消息结构（contracts 增量）

`thread.turn.start` 的 message 增加一个可选字段（命名草案 `freshContext`）：

```
message: {
  text: string                     // 头部 + round 块 + 尾部（恒投内容）
  freshContext?: string            // 可选；仅 fresh 时由 T3 拼在 text 最前
}
```

- **fresh 拼装**（T3 判定 provider 线程无历史）：`persona（T3 settings，仅此时注入） + freshContext + text`。
- **resumed 拼装**：`text` 原样。persona 不注入（历史已含）。
- 字段缺失（老桥/未启用）→ T3 直接投 `text`，行为不变。

### 块内划分

| 内容 | 归属 | 理由 |
|---|---|---|
| dutyPreamble 头部（身份+指针，无工作区） | text（恒投） | 裁决②：两轮都用全量头部 |
| meta 稳定字段（missionId/origin work/member stations/parent） | freshContext | 每轮完全相同，裁决⑤ |
| Objective | freshContext | mission 创建即定，不变 |
| revision | 站行（text） | 每轮变；missionId 已由头部指针+尾部命令携带 |
| 站行/到站/incoming feedback/reason | text | 每轮的事实 |
| Current step | text | 每轮变 |
| Station charter（sha 前缀） | text | 一行开销小；章程若中途变更须对 resumed 可见 |
| Outcomes → routes | text | 决策关键，恒可见最稳 |
| Documents（含 receipt 前缀） | text | receipt 每次写入都变 |
| 尾部 reminder | text | recency 锚，每轮必发（不变） |

头部草案（v2，无工作区）：

```
You are the InfiniteMission member **t3-supervisor**（实施主管）. A mission brief follows — do the work it asks for, then close your round; the reminder after the brief owns the rules for replying.

Re-read: `im mission show ms_f7df121cfe17b9b0e94a5e87f6746ba0 --for t3-supervisor` · Documents & full flags: `im help`

---
```

## 4. T3 判定谓词（fresh ⇔ 拼 freshContext + 注入 persona）

fresh 当且仅当本回合的 provider 会话**既非存量活会话复用、也非携带可用 resumeCursor 的重启**：

- 活会话复用（同 provider/instance，含历史）→ resumed；
- 重启但带同 provider 的 resumeCursor（历史随 resume 回来）→ resumed；
- 真·新开（无 cursor）→ fresh；
- **provider/instance 切换 → fresh**（跨 provider 的 cursor 无意义，历史不迁移）；
- 同 provider 内换 model 的重启（现行走保留 cursor 的路径）→ resumed。

判定位置与 `ensureSessionForThread` 同源（那里才有真相）。**时序差自愈**：投递时判 resumed、实际 resume RPC 失败 → turn error → 桥 reopen → 删线程重投 = 新线程 → fresh 全量。不存在「真空上下文收到裁剪块」的持久状态。

persona 门变更：`firstTurnForBridgeThread` 现行判据（session 空或 stopped/error ⇒ fresh）替换为与上述谓词同源的单一人入口；persona 与 freshContext 用**同一个**判定，不各判各的。

## 5. 桥侧改造与退役清单

- `deliver()` 恒发：`text = dutyPreamble + roundBrief + dutyTailReminder`，`freshContext = meta 稳定字段 + Objective`；`briefFromRunView` 一次渲染出 round/full 两种形态（差异=freshContext 部分）。
- 退役：`fullSnapshot` 门、`contextByThread`、`slimPreamble`、`slimBrief`（及其双锚点解析）——桥不再猜测会话状态。
- result 投递（Work-origin 回程，只读、一次性）**维持恒全量**，不进本机制。
- 兼容与部署顺序：contracts 加可选字段（T3 先上，容忍字段缺失=行为不变）→ 桥加配置开关（默认关，维持现行为）→ T3 上线且验证谓词后开开关。回滚=关开关。

## 6. 尺寸与收益

- fresh（mission 首轮/reopen 重投）：persona ~326 + freshContext ~0.6K + text ≈ **~3.2K**，同现状量级。
- resumed 轮：头部 ~330 + round brief ~1.5K（meta 行已裁）+ 尾部 514 ≈ **~2.3K**，且不再向 rollout 堆积 persona/全量拷贝——历史增长显著放缓，persona/头部/Objective 从每轮重复变为仅首轮。

## 7. 验收标准

1. 新 mission 首轮 rollout：含 persona + freshContext（meta/Objective）+ 头部 + 全量 brief（同现行全量形态的信息完备性）。
2. 同 mission 第二轮起：消息以头部开头，无 persona、无 meta 稳定字段、无 Objective；revision（站行）/routes/documents/charter/尾部仍在。
3. provider 中途切换的下一轮：回到 fresh 形态（persona+freshContext 回归）。
4. 人为制造 resume 失败 → turn error → reopen 重投为 fresh。
5. 桥测试套件：slim/fullSnapshot 相关用例替换为两块拼装用例；T3 侧 persona 与 freshContext 判定同源的单入口测试。
6. 全量消息任何形态都不含 workspace 字样（cwd 已携带）。
