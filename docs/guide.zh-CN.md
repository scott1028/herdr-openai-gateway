[繁體中文](guide.md) · [English](guide.en.md) · [简体中文](guide.zh-CN.md) · [日本語](guide.ja.md)

# 使用指南

[README](../README.zh-CN.md) 只讲怎么开始，这里把细节一次讲完。需要时再查就好，不必从头读到尾。

## 名词小抄

第一次接触的话，先记住这几个词就够了：

| 名词 | 大白话 |
| --- | --- |
| herdr | 专门给 AI coding agent 用的终端管理工具。可以把它想成一栋大楼：workspace 是楼层，tab 是房间，pane 是房间里的一张桌子，每张桌子上可以跑一个程序。 |
| agent CLI | 在终端里跑的 AI 助手，这里指 Claude Code（`claude`）和 Codex（`codex`）。 |
| OpenAI 兼容 API | OpenAI 定的一套 HTTP 接口格式。很多工具都会讲这套格式，所以只要模仿它，那些工具就能直接接上来。 |
| transcript | agent CLI 自动写在硬盘上的对话记录（JSONL 文件）。这个 server 从这里读答案。 |
| turn | 一来一回：你发出一条消息，agent 干完活、回你一段话，这算一个 turn。 |

## 你需要准备什么

| 项目 | 说明 |
| --- | --- |
| herdr 0.9 及以上，而且**要先开着** | `serve` 一启动就会去敲 herdr 的门（ping socket）。没人应门就直接退出，错误信息会告诉你它找的是哪个 socket。herdr server protocol 要 20 及以上；更新的版本只会弹警告，照样能用。 |
| Claude Code 或 Codex，装一个或两个都行 | 按你平时的方式安装、登录就好。 |
| herdr 的 codex integration **v8 及以上**（只有用 Codex 才需要） | 运行 `herdr integration install codex` 安装。少了它，server 拿不到 Codex 的 session id，就找不到它的对话记录。 |
| Rust 工具链 | 用来编译。这个 crate 还没放上 crates.io。 |
| Linux 或 macOS | 目前不支持 Windows。 |

## 启动选项

连不上 herdr 时，`serve` 不会硬撑着开一个没用的服务，而是直接报错，告诉你它试了哪个 socket。想连到
别的 herdr 会话，加上 `--herdr-session <NAME>` 或 `--herdr-socket <PATH>`。

配置文件放在 `~/.config/herdr-openai-gateway/config.toml`，有就读，没有就用内置的默认值。常用的参数：

| 参数 | 做什么 |
| --- | --- |
| `--config <PATH>` | 改读别的配置文件 |
| `--listen <ADDR>` | 换一个监听地址（覆盖 `server.listen`） |
| `--herdr-session <NAME>` / `--herdr-socket <PATH>` | 连到别的 herdr 会话或 socket |
| `--log-requests` | 把请求的 header（凭据类会遮掉）和 body 的结构记进日志，消息内容一律不记 |

## 该选哪个 model

`/v1/models` 列出来的不是"正在跑的 agent"，而是"你可以点的菜"。每一项还附了一个非标准的
`description` 字段说明它是哪种模式（opencode 不会读这个字段）。

点了菜不会马上开火。要等对话的第一条消息进来，server 才会在 herdr 开一个 tab、把 CLI 叫起来，然后
替你操作它。之后这段对话都会用同一个 agent，直到客户端开了新的会话（例如 opencode 的 `/new`）。

每个 model 名称后面都会挂一个后缀，区别在于**谁动手执行工具**（改文件、跑命令这类事）：

- `+agent-tools`：整件事外包给 agent。它在自己的 tab 里改文件、跑 shell，该问权限也在那里问，你只会
  拿到最后那段话。
- `+client-tools`：agent 只负责想，动手的是 opencode。agent 说"我要读这个文件"，opencode 用自己的工具
  去读、弹出自己的权限提示和 diff 界面，再把结果交回给 agent。

| id | 模式 | 谁动手 | 客户端看到什么 |
| --- | --- | --- | --- |
| `claude+agent-tools` | self-exec | Claude Code 在 herdr tab 里自己来：改文件、跑 shell、问权限 | 只有最后的文本 |
| `claude+client-tools` | bridge | Claude 自己的工具全关（`--tools ""`），由 opencode 用自己的 read/edit/bash/MCP 工具执行，再把结果送回 | 文本加上 `tool_calls` |
| `codex+agent-tools` | self-exec | Codex 在 herdr tab 里自己来，同上 | 只有最后的文本 |
| `codex+client-tools` | bridge | Codex 留着一个只读 shell（`-s read-only -a never`：只读 sandbox、不问权限），但 bridge 协议会叫它别用；动手的是 opencode | 文本加上 `tool_calls` |

### 两种后缀怎么挑

| | `+agent-tools` | `+client-tools` |
| --- | --- | --- |
| 内部模式 | `ModelMode::SelfExec` | `ModelMode::Bridge` |
| 谁动手 | agent，在它自己的 herdr tab 里 | 客户端（opencode） |
| 客户端拿回什么 | 只有最后的文本 | 文本加上 `tool_calls` |
| opencode 的 `tool_call` 要设 | `false` | `true` |

- 想让 agent 像平时在终端里那样自己干完，你只看结果：选 `+agent-tools`。这个模式完全不管客户端传来的
  `tools`，所以 opencode 界面上不会出现工具调用或 diff，过程都在 herdr 的 tab 里，想看就切过去看。
- 想要 opencode 原本那套工具界面：选 `+client-tools`。用起来就跟接一个普通的 model provider 一样。它靠
  agent 遵守一套文本协议来表达"我要用哪个工具"；如果 agent 回得格式不对，会重试一次，还是不行就直接
  当成纯文本返回。

Claude 的 `+agent-tools` 会通过 PreToolUse HTTP hook 处理 `AskUserQuestion`（Claude 想问你问题时用的
工具）。所以 opencode 能显示每个选项的说明，也支持自由输入、多选和一次问好几题。如果你的 Claude Code
设置里有 `allowedHttpHookUrls`，记得把 `http://127.0.0.1:8787/internal/claude-hooks/pre-tool-use`
（或对应你监听地址的 URL）加进去。没加，或是客户端没提供 `question` 工具时，会改走 herdr TUI 的对话框
转发（见 [agent 弹出对话框时](#agent-弹出对话框时)）。

背后的 Claude agent 启动时会带上 `CLAUDE_CODE_DISABLE_AGENT_VIEW=1`。原因是：在空白的输入行按 ←（或
打 `/bg`），Claude 会把对话搬去后台服务，人就不在 herdr 的 tab 里了。真想要这个行为，可以在 model 的
`env` 里自己覆盖。

### 自动找出你的 shell 命令（discovery）

你可能在 shell 里包了一些自己的命令，例如 `claude-personal` 用另一个账号、`codex-pool` 轮流用几组配置。
server 启动时会开一次你的交互式 shell（`bash` 或 `zsh`），把名字叫 `claude`、`codex`、`claude-*`、
`codex-*` 的 function、alias 和 PATH 上的命令都找出来，自动变成 model，而且两种后缀都有，例如
`claude-personal+agent-tools`、`claude-personal+client-tools`。

找到的候选要满足下面其中一条才会留下：

- 它就是 `claude` 或 `codex` 本身，或者你把名字写进了 `[discovery] include`；
- 它是 function 或 alias，而且内容真的运行了一个独立的 `claude` / `codex`（`command claude`、
  `headroom wrap codex` 算；`codex_flags_ref` 这种只是名字里刚好有 codex 的不算）。

PATH 上名字是 `claude-*` 的普通文件（例如 `claude-desktop` 这种桌面程序）默认会被剔除，除非你写进
`include`。写进 `exclude` 的则一律剔除。

启动方式跟 `herdr pane run` 一样：server 开一个 tab，在 shell 里打 `<command> <args>`，herdr 自己会认出
跑起来的是哪个 CLI。所以你包的命令一定要把 `"$@"` 传下去（例如 `command claude "$@"`），不然 server 塞
进去的 `--session-id` 会半路弄丢。

开着 discovery 的时候，`[[models]]` 不会盖掉整份列表：同一个 id 会覆盖那个 model，新的 id 会多加一个。
discovery 只在启动时看一次 shell，新增 function 之后要重启 `serve` 才会生效。想知道最后会用哪些
model，跑一下 `herdr-openai-gateway check`。

每个 Codex 变体都有自己的 `CODEX_HOME`，herdr integration 也要各自装一次。漏装的话，启动时会弹警告
点名是哪一个：

```bash
CODEX_HOME=~/.codex-personal herdr integration install codex
```

### 实际回答的是哪个模型

决定权在 CLI，不在这个 server。CLI 用它自己的默认模型，或是你写在 model `args` 里的设置。Claude 用
`env.CLAUDE_CONFIG_DIR` 切换 profile，Codex 用 `env.CODEX_HOME`。想固定下来，就在 `[[models]]` 里定义你
自己的 model id。如果设了 `[discovery] enabled = false`，就回到旧规则：只有四个内置 id，除非
`[[models]]` 另外列了别的。

## 对话是怎么对应的

一段客户端对话，对应 herdr 里的一个 agent tab。server 用一把"对话 key"认人，按顺序看这几个地方，
先找到哪个就用哪个：`x-herdr-conversation`、`x-opencode-session-id`、`x-session-id`、body 里的
`prompt_cache_key`，最后是 `user`。

一把 key 都没有的话，就按历史消息比对：如果刚好只有一段"同 model、没带 key、还开着"的对话接得上，
就发去那里；不然就开一段新的。

每次只会把**新的**消息打给 agent。server 记得之前已经发到哪里，只补上差的那一段。非流式的响应会附上
`x-herdr-conversation`、`x-herdr-agent` 和 `x-herdr-tab` 三个 header，让你知道这个请求落在哪个 tab。

如果请求带了 `x-opencode-session-id`，server 会用 Linux 的 `/proc` 找出 loopback 上那个 opencode 进程。
那个进程一退出，大约五秒内就会把它背后的 agent 收掉。之后再 `opencode --continue`，会重放历史、开一个
新 agent。认不出进程的情况（不是 Linux、远程连接，或是共享的 `opencode serve --service`），agent 就按
`idle_ttl_secs` 的规则回收。

其他规则：

- 对话空闲超过 `conversations.idle_ttl_secs`，它的 agent 会被关掉。下一个请求进来时再重新开一个，并重放
  历史。
- `max_agents_per_model` 和 `max_agents_total` 限制同时活着的 agent 数量。超过时，最久没用的对话先被请走。
- 一段对话同一时间只做一件事。上一个 turn 还没结束又发来新请求，会拿到 409 `conversation_busy`。例外是
  上一个客户端已经自己断开了（例如在 opencode 按 Esc）：这时新请求会接手，agent 先收到一个 `esc`；
  Claude 还会再收到一个 `ctrl+c`，不然它会把被打断的那段提示塞回输入框；然后才送进新的提示。
- 带着同一把对话 key、却一条 assistant 消息都没有的请求，会被当成附带请求（例如生成标题、压缩对话），
  直接回 HTTP 409。解决办法见[搭配 opencode](#搭配-opencode) 里的 `small_model`。

### 背后的 tab 是谁的

每个背后的 pane 都会贴一张"名牌"：一个叫 `oa_owner` 的 herdr metadata token，写着是哪个 server 实例
开的。名牌每 30 秒更新一次，最后一次更新后 90 秒没续就失效。

清理残留的 `oa:` tab 时（启动时，以及每次定期巡逻），只要看到名牌写的是另一个还活着的实例，就不会
动它。所以两个 server 可以放心共用同一个 herdr 会话。

## 搭配 opencode

在 `opencode.json` 加上这段：

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "herdr": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "herdr",
      "options": {
        "baseURL": "http://127.0.0.1:8787/v1",
        "headers": { "X-Herdr-Cwd": "/absolute/path/to/your/project" },
        "timeout": false
      },
      "models": {
        "claude+agent-tools": {
          "name": "Claude Code (agent runs its tools)",
          "tool_call": false,
          "limit": { "context": 200000, "output": 8192 }
        },
        "claude+client-tools": {
          "name": "Claude Code (opencode runs tools)",
          "tool_call": true,
          "limit": { "context": 200000, "output": 8192 }
        },
        "codex+agent-tools": {
          "name": "Codex (agent runs its tools)",
          "tool_call": false,
          "limit": { "context": 200000, "output": 8192 }
        },
        "codex+client-tools": {
          "name": "Codex (opencode runs tools)",
          "tool_call": true,
          "limit": { "context": 200000, "output": 8192 }
        }
      }
    }
  },
  "small_model": "<some non-herdr provider/model>"
}
```

几个容易踩的坑：

- `name` 只是 opencode 菜单上显示的名字，写清楚一点，一看就知道是哪种模式。
- `timeout: false` 一定要设。agent 真在干活时，一个 turn 跑好几分钟很正常。
- **千万不要**设 `chunkTimeout`。不管设成 `false` 还是数字，opencode 2.0.28 都会悄悄把整个 provider
  丢掉，`opencode models` 里一个 herdr 的 model 都看不到。
- `X-Herdr-Cwd` 可以不设。设了就以它为准；没设的话，server 会从 opencode 自己的 system prompt（里面的
  `<env>` 区块）抓 `Working directory` 那一行，让 agent 在 opencode 正在用的目录开工。完整的优先顺序是：
  `X-Herdr-Cwd` → 客户端的 `Working directory` → model 的 `cwd` → `defaults.cwd`。不管最后用到哪一个，
  都必须在 `defaults.allowed_cwd_roots` 底下，不然请求会被 403 `cwd_not_allowed` 挡掉，错误信息会告诉你
  这个路径是从哪来的。
- **`small_model` 请设成 herdr 以外的 provider。** opencode 会用同一个 session id 发送生成标题之类的
  附带请求。这些请求等 500 ms 的合并时间后会被 HTTP 409 挡掉，结果只是标题没生成出来，聊天本身不受影响。
- 刚加完 provider 的第一个请求会比较慢，因为 opencode 要先安装 `@ai-sdk/openai-compatible` 这个包。

## agent 弹出对话框时

Claude Code 和 Codex 经常会弹出对话框：启动时问你信不信任这个文件夹、要不要登录，干活干到一半问你能不能
执行某个命令。

server 绝对不会替你点。它会把对话框转给客户端，让你自己选：

- 客户端提供了 opencode 的 `question` 工具时，你会看到一个带着 agent 选项的 `question` 提问。
- 没有的话，你会收到一条列了编号的文本消息，回复你要的编号就好。

你的答案会变成按键送进 agent 的 TUI，然后**原本的 turn 继续往下跑**：原来的消息不会丢，不会另外开
第二个 agent，你的答案也不会被当成新提示打进去。

几种特殊情况：

- 对话框放着不回，超过 `herdr.blocked_timeout_ms` 就会过期。之后才回的答案会收到一条说明，请你重发原来
  的消息。
- 需要打字回答的对话框（没有选项可选）会以文本转给你，告诉你是哪个 herdr tab。去那个 tab 回答就好，
  agent 空下来之后对话会自己接下去。
- 如果你选的答案让 agent 直接退出（例如 "No, exit"），这个回答请求会拿到 502 `backing_agent_exited`，
  那个 tab 也会被关掉。

任何连得到这个 HTTP 端口的人都能回答这些对话框，所以请把这个端口当成一个 shell 来保护。

## 安全

- 默认只绑 `127.0.0.1:8787`，也就是只有你自己这台电脑连得上。没把握就别改。
- 设置 `HERDR_OPENAI_GATEWAY_API_KEY`（或用 `server.api_key_env` 指定别的环境变量名），所有请求都必须带
  `Authorization: Bearer <key>`。没设就不需要 key。
- server 会检查 `Host` 和 `Origin` 是不是对得上绑定的地址。`server.allow_remote = true` 会放宽这项检查，
  是留给你自己部署的 proxy 用的。开了 `allow_remote` 就一定要设 API key，不然 server 会拒绝启动，就算只绑
  loopback 也一样。
- `defaults.allowed_cwd_roots` 决定请求最多能指定到哪些目录。
- self-exec 的 agent 会用你本机的权限在那个目录跑命令、改文件。连得上这个 HTTP 端口的人就能指挥你的
  agent，所以请把这个端口当成一个 shell 来保护。

## 配置文件

[`config.example.toml`](../config.example.toml) 列出了每个选项和默认值，每项都附一行说明。复制到
`~/.config/herdr-openai-gateway/config.toml` 再改就行。写错的 key 会被拒绝，所以拼错字会在启动时直接报错，
不会被悄悄跳过。

## API 参考

| endpoint | 说明 |
| --- | --- |
| `GET /healthz` | 看 herdr 连不连得上；连不上时返回 503 |
| `GET /v1/models` | model 列表；`created` 是 server 的启动时间 |
| `GET /v1/models/{id}` | 单个 model |
| `POST /v1/chat/completions` | 流式、非流式都支持 |

支持的请求字段：`model`、`messages`（`system` / `developer` / `user` / `assistant` / `tool`，以及
`assistant.tool_calls`）、`stream`、`stream_options.include_usage`、`tools`、`tool_choice`、
`parallel_tool_calls`、`user`、`prompt_cache_key`。其他采样参数会照收，但不会生效。`n > 1` 返回 400。

流式的顺序是：role chunk → keepalive 注释 → 内容（每段最多 2 KiB）→ tool-call chunk → finish chunk →
（有要求的话）usage chunk → `[DONE]`。如果流开始后才出错，会先发一个 error 事件，再发 `[DONE]`。

### 错误码

| code | HTTP | 意思 |
| --- | --- | --- |
| `no_new_message` | 400 | 这个请求没带任何新内容 |
| `prompt_too_large` | 400 | 要发出的新内容超过大小上限 |
| `unsupported_role` / `unsupported_parameter` | 400 | 不支持的 role，或 `n > 1` |
| `invalid_api_key` | 401 | 没带 bearer key，或 key 不对 |
| `cwd_not_allowed` | 403 | cwd 不存在，或不在 `allowed_cwd_roots` 底下；信息会告诉你路径从哪来（`X-Herdr-Cwd`、客户端的 `Working directory`、model 的 `cwd` 或 `defaults.cwd`） |
| `origin_not_allowed` | 403 | `Host` / `Origin` 不被允许 |
| `model_not_found` | 404 | 没有这个 model id |
| `conversation_busy` | 409 | 这段对话还有 turn 在跑 |
| `conversation_side_request` | 409 | 对聊天对话发了附带请求，请设置 `small_model`。如果发来时刚好有 turn 在跑，会改为返回 `conversation_busy` |
| `too_many_backing_agents` | 429 | agent 数量到上限了 |
| `backing_agent_exited` | 502 | agent 干到一半挂了，请重发 |
| `agent_start_failed` / `agent_prompt_stalled` / `completion_not_observed` | 502 | 背后的 turn 失败了 |
| `dialog_answer_failed` | 502 | 没办法把对话框的光标移到你选的选项上 |
| `herdr_unavailable` | 503 | herdr 没开，或 socket 路径不对；信息会写出是哪个 socket。herdr 恢复后就能正常用，不用重启 herdr-openai-gateway |
| `agent_startup_blocked` | 503 | 兜底情况：启动时的对话框没办法转给你（屏幕读不到），或卡住超过 `herdr.blocked_timeout_ms` |
| `turn_timeout` | 504 | turn 跑超过 `herdr.turn_timeout_ms` |

## 目前的限制

- `usage` 永远是 0。CLI 的对话记录里没有 token 用量，与其编一个数字让客户端的自动压缩乱判断，不如
  老实给 0。
- 不支持图片输入，图片会被换成 `[image omitted]`。
- 只支持 Claude Code 和 Codex，只能在 Unix 上跑。
- 如果一个 turn 很久都没有输出，客户端可能会用自己的 chunk timeout 把它切断。server 每 10 秒会发一次
  SSE keepalive 注释，但超过几分钟的 turn 还没实际验证过。
- 回复的内容，就是 agent 对话记录里最后那条消息。所以 agent 用一个问题结束这个 turn，或是回答到一半被
  打断，看起来都跟普通回复一样。
