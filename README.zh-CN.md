# InfiniteMission（无限任务）

**`im` —— 纯静态 SQLite 内核上的多 agent 任务编排。**

无守护进程、无服务、无后台常驻。每条命令都是一次性的 CLI 进程，落在一个
WAL 模式的 SQLite 文件上。运行语义是 work-mission 模型：mission 是一次性
邮件，自带合同、在工位之间流转、自带投递历史。

```
cargo install --path .   # 产出二进制：im
```

## 一屏看懂模型

- **工作区** = 目录树里有 `.im/` 的根（向上查找，同 git）。工作区即唯一
  的 project：一切落在 `.im/im.db` + `.im/mission-documents/`。
- **Agent** 用 `im join <id>` 自助注册。重名自动加后缀
  （`alice` → `alice-2`），身份永远不会被冒用。Agent 无状态、无剧本：
  所有持久状态都在 mission 里，作业内容随工位 prompt 与 mission 一起到达。
- **Manager** 是你信任的人类：终端 `im grant <id>`，或 `im ui` 成员页
  （零 manager 时也可以点「设为 manager」——控制台就是人）。manager 创建
  工位和 mission。
- **工作区即 project**：一个目录树一个任务空间，**工位**（`build`、
  `review`、`approve`……）直接挂在工作区上。工位有值守者绑定（随时可换
  绑，last-write-wins）；**不绑值守者 = 用户工位**，即人类的收件箱。
  mission id 由 init 时生成的工作区 uuid 作命名空间。
- **Mission** 由 YAML 模板创建。模板编译为不可变合同：入口工位、各工位
  的 outcome 词表、路由边（`from`/`when`/`to`，可选 `iterationPolicy`）、
  文档声明、按工位的文档权限（read ≠ write）。
- **所有权在工位，不在 agent。** mission 停驻在工位上；只有该工位值守者
  能 submit。换绑只是移动指针——mission、历史、文档都不动。
- **成员之间没有直连。** 没有 `im send`。Agent 互不相连；唯一的邮件是停
  在工位上的 mission。`im receive` 只投递工位到达通知和成员资格通知
  （授权 / 收回 / 删除）。反馈走 round（`--feedback`、`--reason`、文档），
  不是聊天。
- **提交**（`im mission submit`）在单事务里走完整裁决：ended → revision
  CAS → 归属 → 词表 → feedback 必填 → 回执 → 路由（0/1/2+ 边）→ 跳用户
  工位必须给 reason → 落 `round.completed` + `routed` 事件、revision +1。
  `abandon` 永远合法且恒为终局。
- **文档内容寻址**：同样字节 = 同一回执，重试免费。随轮提交的回执证明
  这一轮产出了什么。
- **事件即历史。** `created` / `round.completed` / `routed` / `ended`；
  迭代数是派生的，从不落列。
- **Inbox**：停驻用户工位的 mission，带着发送方必须给出的 reason。由
  manager 处置。

## 快速开始

```bash
mkdir my-project && cd my-project
im init                       # 工作区 + example/pipeline 模板 +
                              # 流水线工位（design/plan/build/review）

im join boss                  # 各 agent 的终端里——不需要任何剧本
im join worker
im grant boss                 # 人类执行：boss 成为 manager

# boss——第一件事：给预建工位绑值守者
# （不绑 = 用户工位，每个跳到那里的 mission 都要等 manager 手动处理）
im work set-executor boss design <agent>
im work set-executor boss plan <agent>
im work set-executor boss build worker
im work set-executor boss review <agent>

# 一条流水线 mission——grill→spec 在 design 岗 agent 自己的 session 对话里
# 完成（零 mission 往返）；聊定后由 design 岗（须为 manager）发起交付：
im mission create <design-agent> --template pipeline --key v1 \
    --objective "ship the widget"          # 蒸馏后的意图

# design 岗第一轮（落定冻结的 spec）：
im receive <design-agent>          # 到达通知
im mission show <ms> --for <design-agent>   # 插值后的章程、权限、词表、revision
im mission doc write <design-agent> <ms> --id spec --file -   # → 回执
im mission submit <design-agent> <ms> --revision 1 --outcome spec-ready \
    --receipts document:<hash>

# worker（goal 到达 build 后）：
im missions worker            # 你值守工位上的全部 active mission
im mission doc read worker <ms> goal.md
im mission doc write worker <ms> --id impl --file src/receipt.md   # → 回执
im mission submit worker <ms> --revision N --outcome done --receipts document:<hash>

# 人类，当其他模板的 mission 跳到用户工位：
im inbox
im mission submit boss <ms> --revision N --outcome ok --reason "同意"
```

工位 prompt 可插值 `{mission.name}`、`{mission.objective}`、
`{mission.from}`、`{mission.iteration}`、`{mission.reason}`（未知槽位保留
字面）。没有角色剧本：外部注册的 agent 只从 mission 简报获得指令。

## 交付流水线

`im init` 预建四个工位并写入 `.im/templates/pipeline.yaml` —— 一条
design→plan→build→review 交付流，纪律蒸馏自
[matt-skills-with-to-goal](https://github.com/tt-a1i/matt-skills-with-to-goal)：

```
人 ⇄ design 岗 agent 的 session      grill→spec 在这里完成：直接对话，
        │                            零 mission 往返
        └─ design 创建 mission ─▶ 落定 spec.md ──spec-ready──▶ plan
                                                          goal-ready │
   design ◀──approved── review ◀──done── build ◀─────────────────────┘

   终审（design）：accept 即终局 │ reject ─▶ build（只修发现的问题）
   回边：spec-gap（plan|review）─▶ design · blocked（build）─▶ plan
        · rework（review）─▶ build
```

- **design** 双阶段：**前期在 mission 之外**——在你自己的 session 对话里
  grill 人、聊出 spec；聊定后（作为 manager）创建 mission、落定冻结的
  SPEC（`spec-ready`）。review 通过后进入**终审**——`accept` 终局，`reject`
  把实现问题打回 build。
- **plan** 把 SPEC 编译成自足的 GOAL（编译，不重访）；spec 不可编译则以
  `spec-gap` 退回。
- **build** 只按 GOAL 施工，逐条完成标准以证据自验，以回执（`impl.md`，含
  施工前 git 基线）交卷；授权边界 = 仅本地实现与验证，不 commit/push/deploy。
- **review** 对照 GOAL 双轴独立验证（完成标准轴 / 仓库规范轴），每条发现必须
  带证据；`rework` 的发现落 `review.md` + `--feedback`。
- 发现走 `--reason`（插值进下一站 prompt）+ `--feedback` + mission 文档——
  没有 peer 通道，流水线也不需要。

四个岗位章程以**工位模版**形式随二进制发布
（`im work create <op> <key> --preset design|plan|build|review`，控制台新建
工位弹窗也有下拉可选），删掉的工位可按模版重建。预建工位就是普通工位：
换绑、改 prompt、退役（`im work unretire` 可重开）、删除都随你。

## 命令面

```
工作区      im init | agents | leave | doctor | clean | ui
收件        im receive <id> [--wait] | pending | history   # 到达通知 + 成员资格，不是聊天
Manager   im grant|revoke <agent> | im managers           # 也可在控制台「成员」页授予
工位        im work create|list|set-executor|set-prompt|retire|unretire
            im work create <op> <key> --preset design|plan|build|review
            im work set-prompt <op> <work> --preset <name>
模板        im template list           （.im/templates/*.yaml）
Mission    im mission create|show|events|end
            im mission submit <agent> <ms> --revision N --outcome O
                                 [--next-node] [--reason] [--feedback] [--receipts]
            im mission abandon <agent> <ms> --revision N [--reason]
            im mission doc read <agent> <ms> <path>
            im mission doc write <agent> <ms> --id <docId> --file <path|->
            im missions <agent>        # 你值守工位上的 active mission
Inbox      im inbox                   # 停在用户工位的 mission
控制台      im ui                      # 浏览器控制台（固定 http://127.0.0.1:4600，仅本机）
```

## 任务模板

```yaml
schemaVersion: 4
name: example
entry: build
works:
  build:
    completion:
      outcomes: [done, need-rework]     # 本工位词表
      terminal: []                      # 在此终局的 outcome
      feedbackRequiredOn: []            # 必须带 --feedback 的 outcome
    documentRights:                     # write 不蕴含 read
      read: [spec]
      write: [impl]
  review:
    completion: {outcomes: [pass, fail], terminal: [pass], feedbackRequiredOn: [fail]}
    documentRights: {read: [impl], write: [notes]}
paths:
  - {from: build, when: done, to: review}
  - {from: review, when: fail, to: build, iterationPolicy: increment}
  - {from: review, when: any, to: approve}
documents:
  - {id: spec, kind: file, path: docs/spec.md}
  - {id: impl, kind: file, path: docs/impl.md}
  - {id: notes, kind: file, path: docs/notes.md}
```

校验 fail-closed：保留字 `abandon`、终局 outcome 带出边、词表外 `when`、
未声明文档 id、路径点/空段——全部在编译期拒绝，mission 不会诞生。

## 为什么是"静态内核"？

服务该做的事——路由、CAS、裁决、投递历史、通知扇出——全部发生在每次
CLI 调用的单个 SQLite 事务里。agent 来来去去、终端死掉、身份换后缀：
数据库是唯一事实源，每条命令都从它重新推导世界。除了 `im clean`，没有什么
能杀掉 mission。

## 控制台

`im ui` 启动临时 localhost 控制台（固定默认端口 4600、自动开浏览器、闲置 5 分钟自退；`--port N`
可换）：工位板（含换绑；新建弹窗可选流水线岗位模版预填章程）、mission 视图
（revision/at/disposition）、用户 inbox（带跳转 reason；outcome 按钮会弹框让你
填 `--reason`，路由到用户工位时必填）、投递历史时间线、成员管理（授权/收回
manager、删除——零 manager 时也可用，控制台即人类）、任务动作（从模板创建、
终结）。

## 出处

无守护底座（工作区发现、agent 注册、会话令牌、通知锁/等待循环、临时 UI
服务器）派生自 [squad](https://github.com/mco-org/squad)
（MIT）。mission 语义遵循 work-mission 信箱模型。MIT 许可。
