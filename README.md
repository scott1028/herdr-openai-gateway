[繁體中文](README.md) · [English](README.en.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja.md)

# herdr-openai-gateway

讓只會講 OpenAI API 的工具（例如 opencode），也能叫你電腦上的 Claude Code 或 Codex 幫忙做事。

把它想成一個櫃檯：工具把問題交給它，它走進 [herdr](https://herdr.dev)（一個管理終端機的工具），
在分頁裡把問題打給 Claude Code 或 Codex，等對方回完，再把答案拿回來。

```text
opencode  ──問題──▶  herdr-openai-gateway  ──打字──▶  herdr 裡的 claude / codex
          ◀──答案──                        ◀──讀對話記錄──
```

CLI 就跑在你看得到的終端機裡，想看隨時可以切過去，必要時也能自己動手。

> **先講清楚：** 這樣用不代表符合 provider 的服務條款。拿個人訂閱替其他工具回答問題可能違規，請自己確認。

## 需要什麼

- herdr 0.9 以上，而且要先開著
- Claude Code 或 Codex，已經登入。用 Codex 的話，再跑一次 `herdr integration install codex`
- Rust（用來編譯）
- Linux 或 macOS

## 裝起來、跑起來

```bash
cargo install --path .
herdr-openai-gateway check   # 確認連得到 herdr
herdr-openai-gateway serve   # 開始服務：http://127.0.0.1:8787/v1
```

## 試一下

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -H 'X-Herdr-Cwd: /absolute/path/to/your/project' \
  -d '{"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}]}'
```

切到 herdr，會看到一個叫 `openai-api` 的 workspace，Claude Code 就在裡面幫你做事。

## 選哪個 model

model 名稱後面的後綴，決定誰來動手（改檔案、跑指令）：

| 後綴 | 誰動手 | 你會看到 |
| --- | --- | --- |
| `+agent-tools` | agent 在 herdr 裡自己做完 | 只有最後的回答 |
| `+client-tools` | agent 出主意，opencode 動手 | opencode 平常的工具畫面和 diff |

內建四個：`claude+agent-tools`、`claude+client-tools`、`codex+agent-tools`、`codex+client-tools`。
你在 shell 裡包的指令（例如 `claude-personal`）也會自動出現在清單裡。

## 接上 opencode

在 `opencode.json` 加上這段（Codex 的兩個 model 照樣加）：

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

三件事一定要記得：

- `timeout: false` 一定要設，agent 做事常常要好幾分鐘。
- 不要設 `chunkTimeout`，不然 opencode 2.0.28 會讓整個 provider 消失。
- `small_model` 要設成 herdr 以外的 provider，不然產生標題的請求會被擋掉（聊天本身不受影響）。

## 安全

- 預設只聽 `127.0.0.1`，只有你自己的電腦連得到。
- 想加一道鎖：設定 `HERDR_OPENAI_GATEWAY_API_KEY`，之後每個請求都要帶 `Authorization: Bearer <key>`。
- 連得到這個 port 的人，就能叫你的 agent 跑指令、改檔案。請像保護 shell 一樣保護它。

## 想知道更多

[使用指南](docs/guide.md)有完整說明，包括：

- 啟動旗標與設定檔
- model 的細節，以及怎麼自動找出你的 shell 指令
- 對話怎麼對應到 herdr 的 tab、閒置多久會被收掉
- agent 跳出對話框時怎麼回答
- API 欄位與錯誤碼
- 目前的限制

## 授權

Apache-2.0。
