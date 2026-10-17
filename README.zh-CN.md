[繁體中文](README.md) · [English](README.en.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja.md)

# herdr-openai-gateway

让只会讲 OpenAI API 的工具（例如 opencode），也能叫你电脑上的 Claude Code 或 Codex 帮忙干活。

把它想成一个前台：工具把问题交给它，它走进 [herdr](https://herdr.dev)（一个管理终端的工具），
在标签页里把问题打给 Claude Code 或 Codex，等对方回完，再把答案拿回来。

```text
opencode  ──问题──▶  herdr-openai-gateway  ──打字──▶  herdr 里的 claude / codex
          ◀──答案──                        ◀──读对话记录──
```

CLI 就跑在你看得到的终端里，想看随时可以切过去，必要时也能自己动手。

> **先讲清楚：** 这样用不代表符合 provider 的服务条款。拿个人订阅替其他工具回答问题可能违规，请自己确认。

## 需要什么

- herdr 0.9 及以上，而且要先开着
- Claude Code 或 Codex，已经登录。用 Codex 的话，再跑一次 `herdr integration install codex`
- Rust（用来编译）
- Linux 或 macOS

## 装起来、跑起来

```bash
cargo install --path .
herdr-openai-gateway check   # 确认连得上 herdr
herdr-openai-gateway serve   # 开始服务：http://127.0.0.1:8787/v1
```

## 试一下

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -H 'X-Herdr-Cwd: /absolute/path/to/your/project' \
  -d '{"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}]}'
```

切到 herdr，会看到一个叫 `openai-api` 的 workspace，Claude Code 就在里面帮你干活。

## 选哪个 model

model 名称后面的后缀，决定谁来动手（改文件、跑命令）：

| 后缀 | 谁动手 | 你会看到 |
| --- | --- | --- |
| `+agent-tools` | agent 在 herdr 里自己干完 | 只有最后的回答 |
| `+client-tools` | agent 出主意，opencode 动手 | opencode 平常的工具界面和 diff |

内置四个：`claude+agent-tools`、`claude+client-tools`、`codex+agent-tools`、`codex+client-tools`。
你在 shell 里包的命令（例如 `claude-personal`）也会自动出现在列表里。

## 接上 opencode

在 `opencode.json` 加上这段（Codex 的两个 model 照样加）：

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "herdr": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "herdr",
      "options": { "baseURL": "http://127.0.0.1:8787/v1", "timeout": false },
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
        }
      }
    }
  },
  "small_model": "<some non-herdr provider/model>"
}
```

三件事一定要记得：

- `timeout: false` 一定要设，agent 干活常常要好几分钟。
- 不要设 `chunkTimeout`，不然 opencode 2.0.28 会让整个 provider 消失。
- `small_model` 要设成 herdr 以外的 provider，不然生成标题的请求会被挡掉（聊天本身不受影响）。

## 安全

- 默认只监听 `127.0.0.1`，只有你自己的电脑连得上。
- 想加一道锁：设置 `HERDR_OPENAI_GATEWAY_API_KEY`，之后每个请求都要带 `Authorization: Bearer <key>`。
- 连得上这个端口的人，就能叫你的 agent 跑命令、改文件。请像保护 shell 一样保护它。

## 想知道更多

[使用指南](docs/guide.zh-CN.md)有完整说明，包括：

- 启动参数与配置文件
- model 的细节，以及怎么自动找出你的 shell 命令
- 对话怎么对应到 herdr 的 tab、空闲多久会被回收
- agent 弹出对话框时怎么回答
- API 字段与错误码
- 目前的限制

## 许可证

Apache-2.0。
