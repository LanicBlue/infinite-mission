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
- **Agent** 用 `im join <id> --role <role>` 自助注册。重名自动加后缀
  （`alice` → `alice-2`），身份永远不会被冒用。Agent 无状态：所有持久
  状态都在 mission 里。
- **Operator** 是你信任的人类：`im grant <id>`。operator 创建工位和
  mission。
- **工作区即 project**：一个目录树一个任务空间，**工位**（`build`、
  `review`、`approve`……）直接挂在工作区上。工位有值守者绑定（随时可换
  绑，last-write-wins）；**不绑值守者 = 用户工位**，即人类的收件箱。
  mission id 由 init 时生成的工作区 uuid 作命名空间。
- **Mission** 由 YAML 模板创建。模板编译为不可变合同：入口工位、各工位
  的 outcome 词表、路由边（`from`/`when`/`to`，可选 `iterationPolicy`）、
  文档声明、按工位的文档权限（read ≠ write）。
- **所有权在工位，不在 agent。** mission 停驻在工位上；只有该工位值守者
  能 submit。换绑只是移动指针——mission、历史、文档都不动。
- **提交**（`im mission submit`）在单事务里走完整裁决：ended → revision
  CAS → 归属 → 词表 → feedback 必填 → 回执 → 路由（0/1/2+ 边）→ 跳用户
  工位必须给 reason → 落 `round.completed` + `routed` 事件、revision +1。
  `abandon` 永远合法且恒为终局。
- **文档内容寻址**：同样字节 = 同一回执，重试免费。随轮提交的回执证明
  这一轮产出了什么。
- **事件即历史。** `created` / `round.completed` / `routed` / `ended`；
  迭代数是派生的，从不落列。
- **Inbox**：停驻用户工位的 mission，带着发送方必须给出的 reason。由
  operator 处置。

## 快速开始

```bash
mkdir my-project && cd my-project
im init                       # 工作区、角色、示例模板、/im 装机

im join boss --role manager   # 各 agent 的终端里
im join worker --role worker
im grant boss                 # 人类执行：boss 成为 operator

# boss：
im work create boss build --executor worker
im work create boss review --executor reviewer
im work create boss approve          # 用户工位——不绑值守者
im mission create boss --template example --key v1

# worker：
im receive worker             # 到达通知：有 mission 落到你值守的工位
im missions worker            # 你值守工位上的全部 active mission
im mission show <ms> --for worker   # prompt、文档权限、词表、路由表、revision
im mission doc write worker <ms> --id impl --file src/impl.md   # → 回执
im mission submit worker <ms> --revision 1 --outcome done \
    --receipts document:<hash>

# 人类，当 mission 跳到用户工位：
im inbox
im mission submit boss <ms> --revision 2 --outcome ok
```

`im setup`（`im init` 已顺带执行）为 Claude Code / Gemini CLI / Codex /
OpenCode 装上斜杠命令：`/im worker`、`/im boss`……教会宿主 agent 该角色的
动词环。

## 命令面

```
工作区      im init | agents | leave | roles | setup | doctor | clean | ui
消息层      im send <from> <to|@all> <text> | receive <id> [--wait] | pending | history
Operator   im grant|revoke <agent> | operators
工位        im work create|list|set-executor|set-prompt|retire
模板        im template list           （.im/templates/*.yaml）
Mission    im mission create|show|events|end
            im mission submit <agent> <ms> --revision N --outcome O
                                 [--next-node] [--reason] [--feedback] [--receipts]
            im mission abandon <agent> <ms> --revision N [--reason]
            im mission doc read <agent> <ms> <path>
            im mission doc write <agent> <ms> --id <docId> --file <path|->
            im missions <agent>        # 你值守工位上的 active mission
Inbox      im inbox                   # 停在用户工位的 mission
控制台      im ui                      # 临时浏览器控制台（localhost）
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

`im ui` 启动临时 localhost 控制台（随机端口、自动开浏览器、闲置 5 分钟自
退）：工位板（含换绑）、mission 视图（revision/at/disposition）、用户
inbox（带跳转 reason）、投递历史时间线、operator 动作（授权、从模板建
mission、终结）。

## 出处

无守护底座（工作区发现、agent 注册、会话令牌、消息锁/等待循环、斜杠命令
装机、临时 UI 服务器）派生自 [squad](https://github.com/mco-org/squad)
（MIT）。mission 语义遵循 work-mission 信箱模型。MIT 许可。
