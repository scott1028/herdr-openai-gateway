[繁體中文](guide.md) · [English](guide.en.md) · [简体中文](guide.zh-CN.md) · [日本語](guide.ja.md)

# 使用指南

[README](../README.md) 只講怎麼開始，這裡把細節一次講完。需要時再查就好，不必從頭讀到尾。

## 名詞小抄

第一次接觸的話，先記住這幾個詞就夠了：

| 名詞 | 白話解釋 |
| --- | --- |
| herdr | 專門給 AI coding agent 用的終端機管理工具。可以把它想成一棟大樓：workspace 是樓層，tab 是房間，pane 是房間裡的一張桌子，每張桌子上可以跑一個程式。 |
| agent CLI | 在終端機裡跑的 AI 助手，這裡指 Claude Code（`claude`）和 Codex（`codex`）。 |
| OpenAI 相容 API | OpenAI 定的一套 HTTP 介面格式。很多工具都會講這套格式，所以只要模仿它，那些工具就能直接接上來。 |
| transcript | agent CLI 自動寫在硬碟上的對話記錄（JSONL 檔）。這個 server 從這裡讀答案。 |
| turn | 一來一回：你送出一則訊息，agent 做完事、回你一段話，這樣算一個 turn。 |

## 你需要準備什麼

| 項目 | 說明 |
| --- | --- |
| herdr 0.9 以上，而且**要先開著** | `serve` 一啟動就會去敲 herdr 的門（ping socket）。沒人應門就直接結束，錯誤訊息會告訴你它找的是哪個 socket。herdr server protocol 要 20 以上；更新的版本只會跳警告，照樣能用。 |
| Claude Code 或 Codex，裝一個或兩個都行 | 照你平常的方式安裝、登入就好。 |
| herdr 的 codex integration **v8 以上**（只有用 Codex 才需要） | 執行 `herdr integration install codex` 安裝。少了它，server 拿不到 Codex 的 session id，就找不到它的對話記錄。 |
| Rust toolchain | 用來編譯。這個 crate 還沒放上 crates.io。 |
| Linux 或 macOS | 目前不支援 Windows。 |

## 啟動選項

連不上 herdr 時，`serve` 不會硬撐著開一個沒用的服務，而是直接報錯，告訴你它試了哪個 socket。想連
到別的 herdr session，加上 `--herdr-session <NAME>` 或 `--herdr-socket <PATH>`。

設定檔放在 `~/.config/herdr-openai-gateway/config.toml`，有就讀，沒有就用內建的預設值。常用的旗標：

| 旗標 | 做什麼 |
| --- | --- |
| `--config <PATH>` | 改讀別的設定檔 |
| `--listen <ADDR>` | 換一個監聽位址（覆寫 `server.listen`） |
| `--herdr-session <NAME>` / `--herdr-socket <PATH>` | 連到別的 herdr session 或 socket |
| `--log-requests` | 把請求的 header（帳密類的會遮掉）和 body 的結構記進 log，訊息內容一律不記 |

## 該選哪個 model

`/v1/models` 列出來的不是「正在跑的 agent」，而是「你可以點的菜」。每個項目還附了一個非標準的
`description` 欄位說明它是哪種模式（opencode 不會讀這個欄位）。

點了菜不會馬上開火。要等對話的第一則訊息進來，server 才會在 herdr 開一個 tab、把 CLI 叫起來，然後
替你操作它。之後這段對話都會用同一個 agent，直到 client 開了新的 session（例如 opencode 的 `/new`）。

每個 model 名稱後面都會掛一個後綴，差別在於**誰動手執行工具**（改檔案、跑指令這類事）：

- `+agent-tools`：整件事外包給 agent。它在自己的 tab 裡改檔、跑 shell，該問權限也在那裡問，你只會
  拿到最後那段話。
- `+client-tools`：agent 只負責想，動手的是 opencode。agent 說「我要讀這個檔」，opencode 用自己的工具
  去讀、跳出自己的權限提示和 diff 畫面，再把結果交回給 agent。

| id | 模式 | 誰動手 | client 看到什麼 |
| --- | --- | --- | --- |
| `claude+agent-tools` | self-exec | Claude Code 在 herdr tab 裡自己來：改檔、跑 shell、問權限 | 只有最後的文字 |
| `claude+client-tools` | bridge | Claude 自己的工具全關（`--tools ""`），由 opencode 用自己的 read/edit/bash/MCP 工具執行，再把結果送回 | 文字加上 `tool_calls` |
| `codex+agent-tools` | self-exec | Codex 在 herdr tab 裡自己來，同上 | 只有最後的文字 |
| `codex+client-tools` | bridge | Codex 留著一個唯讀 shell（`-s read-only -a never`：唯讀 sandbox、不問權限），但 bridge 協定會叫它別用；動手的是 opencode | 文字加上 `tool_calls` |

### 兩種後綴怎麼挑

| | `+agent-tools` | `+client-tools` |
| --- | --- | --- |
| 內部模式 | `ModelMode::SelfExec` | `ModelMode::Bridge` |
| 誰動手 | agent，在它自己的 herdr tab 裡 | client（opencode） |
| client 拿回什麼 | 只有最後的文字 | 文字加上 `tool_calls` |
| opencode 的 `tool_call` 要設 | `false` | `true` |

- 想讓 agent 像平常在終端機裡那樣自己做完，你只看結果：選 `+agent-tools`。這個模式完全不管 client
  傳來的 `tools`，所以 opencode 畫面上不會出現工具呼叫或 diff，過程都在 herdr 的 tab 裡，想看就切過去看。
- 想要 opencode 原本那套工具介面：選 `+client-tools`。用起來就跟接一般的 model provider 一樣。它靠
  agent 遵守一套文字協定來表達「我要用哪個工具」；如果 agent 回得格式不對，會重試一次，還是不行就直接
  當成純文字回傳。

Claude 的 `+agent-tools` 會透過 PreToolUse HTTP hook 處理 `AskUserQuestion`（Claude 想問你問題時用的
工具）。所以 opencode 能顯示每個選項的說明，也支援自由輸入、多選和一次問好幾題。如果你的 Claude Code
設定裡有 `allowedHttpHookUrls`，記得把 `http://127.0.0.1:8787/internal/claude-hooks/pre-tool-use`
（或對應你監聽位址的 URL）加進去。沒加，或是 client 沒提供 `question` 工具時，會改走 herdr TUI 的對話框
轉送（見 [agent 跳出對話框時](#agent-跳出對話框時)）。

背後的 Claude agent 啟動時會帶上 `CLAUDE_CODE_DISABLE_AGENT_VIEW=1`。原因是：在空白的輸入列按 ←（或
打 `/bg`），Claude 會把對話搬去背景服務，人就不在 herdr 的 tab 裡了。真的想要這個行為，可以在 model
的 `env` 裡自己覆寫。

### 自動找出你的 shell 指令（discovery）

你可能在 shell 裡包了一些自己的指令，例如 `claude-personal` 用另一個帳號、`codex-pool` 輪流用幾組設定。
server 啟動時會開一次你的互動式 shell（`bash` 或 `zsh`），把名字叫 `claude`、`codex`、`claude-*`、
`codex-*` 的 function、alias 和 PATH 上的指令都找出來，自動變成 model，而且兩種後綴都有，例如
`claude-personal+agent-tools`、`claude-personal+client-tools`。

找到的候選要符合下面其中一條才會留下：

- 它就是 `claude` 或 `codex` 本人，或是你把名字寫進了 `[discovery] include`；
- 它是 function 或 alias，而且內容真的有執行一個獨立的 `claude` / `codex`（`command claude`、
  `headroom wrap codex` 算；`codex_flags_ref` 這種只是名字裡剛好有 codex 的不算）。

PATH 上名字是 `claude-*` 的一般檔案（例如 `claude-desktop` 這種桌面程式）預設會被剔除，除非你寫進
`include`。寫進 `exclude` 的則一律剔除。

啟動方式跟 `herdr pane run` 一樣：server 開一個 tab，在 shell 裡打 `<command> <args>`，herdr 自己會認出
跑起來的是哪個 CLI。所以你包的指令一定要把 `"$@"` 傳下去（例如 `command claude "$@"`），不然 server 塞
進去的 `--session-id` 會半路弄丟。

開著 discovery 的時候，`[[models]]` 不會蓋掉整份清單：同一個 id 會覆寫那個 model，新的 id 會多加一個。
discovery 只在啟動時看一次 shell，新增 function 之後要重開 `serve` 才會生效。想知道最後會用哪些
model，跑一下 `herdr-openai-gateway check`。

每個 Codex 變體都有自己的 `CODEX_HOME`，herdr integration 也要各自裝一次。漏裝的話，啟動時會跳警告
點名是哪一個：

```bash
CODEX_HOME=~/.codex-personal herdr integration install codex
```

### 實際回答的是哪個模型

決定權在 CLI，不在這個 server。CLI 用它自己的預設模型，或是你寫在 model `args` 裡的設定。Claude 用
`env.CLAUDE_CONFIG_DIR` 切換 profile，Codex 用 `env.CODEX_HOME`。想固定下來，就在 `[[models]]` 裡定義你
自己的 model id。如果設了 `[discovery] enabled = false`，就回到舊規則：只有四個內建 id，除非
`[[models]]` 另外列了別的。

## 對話是怎麼對應的

一段 client 對話，對應 herdr 裡的一個 agent tab。server 用一把「對話 key」認人，依序看這幾個地方，
先找到哪個就用哪個：`x-herdr-conversation`、`x-opencode-session-id`、`x-session-id`、body 裡的
`prompt_cache_key`，最後是 `user`。

key 裡面不含 model，所以同一把 key 永遠落在同一段對話。中途換 model 只會換掉背後那個 CLI：舊的 tab
關掉、開一個新的，歷史完整重播，對話 id 和 key 都不變。

一把 key 都沒有的話，就看歷史訊息比對：如果剛好只有一段「同 model、沒帶 key、還開著」的對話接得上，
就送去那裡；不然就開一段新的。

每次只會把**新的**訊息打給 agent。server 記得之前已經送到哪裡，只補上差的那一段。非串流的回應會附上
`x-herdr-conversation`、`x-herdr-agent` 和 `x-herdr-tab` 三個 header，讓你知道這個請求落在哪個 tab。

如果請求帶了 `x-opencode-session-id`，server 會用 Linux 的 `/proc` 找出 loopback 上那個 opencode 行程。
那個行程一結束，大約五秒內就會把它背後的 agent 收掉。之後再 `opencode --continue`，會重播歷史、開一個
新 agent。認不出行程的情況（不是 Linux、遠端連線，或是共用的 `opencode serve --service`），agent 就照
`idle_ttl_secs` 的規則收。

其他規則：

- 對話閒置超過 `conversations.idle_ttl_secs`，它的 agent 會被關掉。下一個請求進來時再重新開一個，並重播
  歷史。
- `max_agents_per_model` 和 `max_agents_total` 限制同時活著的 agent 數量。超過時，最久沒用的對話先被請走。
- 一段對話同一時間只做一件事。上一個 turn 還沒結束又送來新請求，會拿到 409 `conversation_busy`。例外是
  上一個 client 已經自己掛斷了（例如在 opencode 按 Esc）：這時新請求會接手，agent 先收到一個 `esc`；
  Claude 還會再收到一個 `ctrl+c`，不然它會把被打斷的那段提示塞回輸入框；然後才送進新的提示。
- 帶著同一把對話 key、卻一則 assistant 訊息都沒有的請求，會被當成附帶請求（例如產生標題、壓縮對話），
  直接回 HTTP 409。解法見[搭配 opencode](#搭配-opencode) 裡的 `small_model`。key 不認 model，所以
  `small_model` 指到本 server 的另一個 model 時，標題請求和主請求還是同一把 key：標題請求先到就會先
  占住這段對話，主請求那一下會拿到 409。

### 背後的 tab 是誰的

每個背後的 pane 都會貼一張「名牌」：一個叫 `oa_owner` 的 herdr metadata token，寫著是哪個 server
instance 開的。名牌每 30 秒更新一次，最後一次更新後 90 秒沒續就失效。

清理殘留的 `oa:` tab 時（啟動時，以及每次定期巡邏），只要看到名牌寫的是另一個還活著的 instance，就
不會動它。所以兩個 server 可以放心共用同一個 herdr session。

## 搭配 opencode

在 `opencode.json` 加上這段：

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

幾個容易踩的坑：

- `name` 只是 opencode 選單上顯示的名字，寫清楚一點，一看就知道是哪種模式。
- `timeout: false` 一定要設。agent 真的在做事時，一個 turn 跑好幾分鐘很正常。
- **千萬不要**設 `chunkTimeout`。不管設成 `false` 還是數字，opencode 2.0.28 都會默默把整個 provider
  丟掉，`opencode models` 裡一個 herdr 的 model 都看不到。
- `X-Herdr-Cwd` 可以不設。有設就以它為準；沒設的話，server 會從 opencode 自己的 system prompt（裡面的
  `<env>` 區塊）抓 `Working directory` 那一行，讓 agent 在 opencode 正在用的目錄開工。完整的優先順序是：
  `X-Herdr-Cwd` → client 的 `Working directory` → model 的 `cwd` → `defaults.cwd`。不管最後用到哪一個，
  都必須在 `defaults.allowed_cwd_roots` 底下，不然請求會被 403 `cwd_not_allowed` 擋掉，錯誤訊息會告訴你
  這個路徑是從哪裡來的。
- **`small_model` 請設成 herdr 以外的 provider。** opencode 會用同一個 session id 送出產生標題之類的
  附帶請求。這些請求等 500 ms 的合併時間後會被 HTTP 409 擋掉，結果只是標題沒產生出來，聊天本身不受影響。
- 剛加完 provider 的第一個請求會比較慢，因為 opencode 要先安裝 `@ai-sdk/openai-compatible` 這個套件。

## agent 跳出對話框時

Claude Code 和 Codex 常會跳出對話框：啟動時問你信不信任這個資料夾、要不要登入，做事做到一半問你能不能
執行某個指令。

server 絕對不會替你按。它會把對話框轉到 client 讓你自己選：

- client 有提供 opencode 的 `question` 工具時，你會看到一個帶著 agent 選項的 `question` 提問。
- 沒有的話，你會收到一則列了編號的文字訊息，回覆你要的編號就好。

你的答案會變成按鍵送進 agent 的 TUI，然後**原本的 turn 繼續往下跑**：原來的訊息不會不見，不會另外開
第二個 agent，你的答案也不會被當成新提示打進去。

幾種特殊情況：

- 對話框放著不回，超過 `herdr.blocked_timeout_ms` 就會過期。之後才回的答案會收到一則說明，請你重送原本
  的訊息。
- 需要打字回答的對話框（沒有選項可選）會以文字轉給你，告訴你是哪個 herdr tab。去那個 tab 回答就好，
  agent 空下來之後對話會自己接下去。
- 如果你選的答案讓 agent 直接結束（例如 "No, exit"），這個回答請求會拿到 502 `backing_agent_exited`，
  那個 tab 也會被關掉。

任何連得到這個 HTTP port 的人都能回答這些對話框，所以請把這個 port 當成一個 shell 來保護。

## 安全性

- 預設只綁 `127.0.0.1:8787`，也就是只有你自己這台電腦連得到。沒有把握就別改。
- 設定 `HERDR_OPENAI_GATEWAY_API_KEY`（或用 `server.api_key_env` 指定別的環境變數名稱），所有請求都必須帶
  `Authorization: Bearer <key>`。沒設就不需要 key。
- server 會檢查 `Host` 和 `Origin` 是不是對得上綁定的位址。`server.allow_remote = true` 會放寬這項檢查，
  是留給你自己架的 proxy 用的。開了 `allow_remote` 就一定要設 API key，不然 server 會拒絕啟動，就算只綁
  loopback 也一樣。
- `defaults.allowed_cwd_roots` 決定請求最多能指定到哪些目錄。
- self-exec 的 agent 會用你本機的權限在那個目錄跑指令、改檔案。連得到這個 HTTP port 的人就能指揮你的
  agent，所以請把這個 port 當成一個 shell 來保護。

## 設定檔

[`config.example.toml`](../config.example.toml) 列出了每個選項、預設值，每項都附一行說明。複製到
`~/.config/herdr-openai-gateway/config.toml` 再改就行。寫錯的 key 會被拒絕，所以打錯字會在啟動時直接報錯，
不會被默默略過。

## API 參考

| endpoint | 說明 |
| --- | --- |
| `GET /healthz` | 看 herdr 連不連得到；連不到時回 503 |
| `GET /v1/models` | model 清單；`created` 是 server 的啟動時間 |
| `GET /v1/models/{id}` | 單一 model |
| `POST /v1/chat/completions` | 串流、非串流都支援 |

支援的請求欄位：`model`、`messages`（`system` / `developer` / `user` / `assistant` / `tool`，以及
`assistant.tool_calls`）、`stream`、`stream_options.include_usage`、`tools`、`tool_choice`、
`parallel_tool_calls`、`user`、`prompt_cache_key`。其他取樣參數會照收，但不會生效。`n > 1` 會回 400。

串流的順序是：role chunk → keepalive 註解 → 內容（每段最多 2 KiB）→ tool-call chunk → finish chunk →
（有要求的話）usage chunk → `[DONE]`。如果串流開始後才出錯，會先送一個 error 事件，再送 `[DONE]`。

### 錯誤碼

| code | HTTP | 意思 |
| --- | --- | --- |
| `no_new_message` | 400 | 這個請求沒帶任何新內容 |
| `prompt_too_large` | 400 | 要送出的新內容超過大小上限 |
| `unsupported_role` / `unsupported_parameter` | 400 | 不支援的 role，或 `n > 1` |
| `invalid_api_key` | 401 | 沒帶 bearer key，或 key 不對 |
| `cwd_not_allowed` | 403 | cwd 不存在，或不在 `allowed_cwd_roots` 底下；訊息會告訴你路徑從哪來（`X-Herdr-Cwd`、client 的 `Working directory`、model 的 `cwd` 或 `defaults.cwd`） |
| `origin_not_allowed` | 403 | `Host` / `Origin` 不被允許 |
| `model_not_found` | 404 | 沒有這個 model id |
| `conversation_busy` | 409 | 這段對話還有 turn 在跑 |
| `conversation_side_request` | 409 | 對聊天對話送了附帶請求，請設定 `small_model`。如果送來時剛好有 turn 在跑，會改回 `conversation_busy` |
| `too_many_backing_agents` | 429 | agent 數量到上限了 |
| `backing_agent_exited` | 502 | agent 做到一半掛了，請重送 |
| `agent_start_failed` / `agent_prompt_stalled` / `completion_not_observed` | 502 | 背後的 turn 失敗了 |
| `dialog_answer_failed` | 502 | 沒辦法把對話框的游標移到你選的選項上 |
| `herdr_unavailable` | 503 | herdr 沒開，或 socket 路徑不對；訊息會寫出是哪個 socket。herdr 恢復後就能正常用，不用重開 herdr-openai-gateway |
| `agent_startup_blocked` | 503 | 備援情況：啟動時的對話框沒辦法轉給你（畫面讀不到），或卡住超過 `herdr.blocked_timeout_ms` |
| `turn_timeout` | 504 | turn 跑超過 `herdr.turn_timeout_ms` |

## 目前的限制

- `usage` 永遠是 0。CLI 的對話記錄裡沒有 token 用量，與其假造一個數字讓 client 的自動壓縮亂判斷，不如
  老實給 0。
- 不支援圖片輸入，圖片會被換成 `[image omitted]`。
- 只支援 Claude Code 和 Codex，只能在 Unix 上跑。
- 如果一個 turn 很久都沒有輸出，client 可能會套用自己的 chunk timeout 把它切斷。server 每 10 秒會送一次
  SSE keepalive 註解，但超過幾分鐘的 turn 還沒實際驗證過。
- 回覆的內容，就是 agent 對話記錄裡最後那則訊息。所以 agent 用一個問題結束這個 turn，或是回答到一半被
  打斷，看起來都跟一般回覆一樣。
