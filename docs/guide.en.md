[繁體中文](guide.md) · [English](guide.en.md) · [简体中文](guide.zh-CN.md) · [日本語](guide.ja.md)

# User guide

The [README](../README.en.md) only covers getting started. This page has all the details. Look things up when you need them; there is no need to read it front to back.

## Glossary

If this is all new to you, these few words are enough to get started:

| Term | In plain words |
| --- | --- |
| herdr | A terminal manager built for AI coding agents. Picture a building: a workspace is a floor, a tab is a room, and a pane is a desk in that room. Each desk can run one program. |
| agent CLI | An AI assistant that runs in a terminal. Here that means Claude Code (`claude`) and Codex (`codex`). |
| OpenAI-compatible API | The HTTP interface format OpenAI defined. Lots of tools speak it, so anything that imitates it can plug straight into those tools. |
| transcript | The conversation log (a JSONL file) an agent CLI writes to disk on its own. This server reads answers from it. |
| turn | One round trip: you send a message, the agent does its work and replies. |

## What you need

| Item | Notes |
| --- | --- |
| herdr 0.9 or newer, **already running** | As soon as `serve` starts, it knocks on herdr's door (pings the socket). If nobody answers, it exits, and the error tells you which socket it tried. The herdr server must speak protocol 20 or newer; newer protocols only print a warning and still work. |
| Claude Code, Codex, or both | Install and sign in the way you normally do. |
| herdr's codex integration **v8 or newer** (only if you use Codex) | Install it with `herdr integration install codex`. Without it, the server cannot get Codex's session id, so it cannot find Codex's session log. |
| Rust toolchain | For building. The crate is not on crates.io yet. |
| Linux or macOS | Windows is not supported yet. |

## Startup options

If herdr is unreachable, `serve` does not limp along with a useless service. It reports the error
right away and tells you which socket it tried. To use a different herdr session, add
`--herdr-session <NAME>` or `--herdr-socket <PATH>`.

The config file lives at `~/.config/herdr-openai-gateway/config.toml`. If it exists it is read;
otherwise the built-in defaults are used. Handy flags:

| Flag | What it does |
| --- | --- |
| `--config <PATH>` | Read a different config file |
| `--listen <ADDR>` | Listen on another address (overrides `server.listen`) |
| `--herdr-session <NAME>` / `--herdr-socket <PATH>` | Connect to another herdr session or socket |
| `--log-requests` | Log request headers (credentials are masked) and the body's shape; message text is never logged |

## Which model to pick

`/v1/models` is not a list of "agents that are running". It is a menu of what you can order. Each
entry also carries a non-standard `description` field that says which mode it is (opencode does
not read that field).

Ordering does not start the cooking. Only when the first message of a conversation arrives does
the server open a tab in herdr, start the CLI there, and drive it for you. The conversation then
keeps the same agent until the client starts a new session (for example opencode's `/new`).

Every model name ends with a suffix, and the difference is **who does the hands-on work** with
tools (editing files, running commands, and so on):

- `+agent-tools`: you hand the whole job to the agent. It edits files and runs the shell in its
  own tab, asks for permissions there too, and you only get its final message.
- `+client-tools`: the agent only does the thinking, and opencode does the hands-on work. The
  agent says "I want to read this file"; opencode reads it with its own tools, shows its own
  permission prompts and diff view, and hands the result back to the agent.

| id | mode | who does the work | what the client sees |
| --- | --- | --- | --- |
| `claude+agent-tools` | self-exec | Claude Code does it all in the herdr tab: edits files, runs the shell, asks for permissions | only the final text |
| `claude+client-tools` | bridge | Claude's own tools are all off (`--tools ""`); opencode runs its own read/edit/bash/MCP tools and sends the results back | text plus `tool_calls` |
| `codex+agent-tools` | self-exec | Codex does it all in the herdr tab, as above | only the final text |
| `codex+client-tools` | bridge | Codex keeps a read-only shell (`-s read-only -a never`: read-only sandbox, never asks), but the bridge protocol tells it not to use it; opencode does the work | text plus `tool_calls` |

### Choosing between the two suffixes

| | `+agent-tools` | `+client-tools` |
| --- | --- | --- |
| internal mode | `ModelMode::SelfExec` | `ModelMode::Bridge` |
| who does the work | the agent, in its own herdr tab | the client (opencode) |
| what the client gets back | only the final text | text plus `tool_calls` |
| set opencode's `tool_call` to | `false` | `true` |

- Want the agent to finish the job on its own, the way it does in a terminal, while you just look
  at the result? Pick `+agent-tools`. This mode ignores the `tools` the client sends, so opencode
  shows no tool calls or diffs. Everything happens in the herdr tab; switch over if you want to
  watch.
- Want opencode's usual tool interface? Pick `+client-tools`. It feels like any ordinary model
  provider. It relies on the agent following a text protocol to say which tool it wants. If the
  agent's reply has the wrong format, it is retried once, and after that it is returned as plain
  text.

Claude's `+agent-tools` handles `AskUserQuestion` (the tool Claude uses when it wants to ask you
something) through a PreToolUse HTTP hook. That is why opencode can show a description for each
option and supports free text, multiple selection, and several questions at once. If your Claude
Code settings contain `allowedHttpHookUrls`, remember to add
`http://127.0.0.1:8787/internal/claude-hooks/pre-tool-use` (or the URL for your listen address).
Without it, or when the client does not offer the `question` tool, it falls back to relaying the
herdr TUI dialog (see [When the agent shows a dialog](#when-the-agent-shows-a-dialog)).

The Claude agents behind the scenes start with `CLAUDE_CODE_DISABLE_AGENT_VIEW=1`. The reason:
pressing ← on an empty input line (or typing `/bg`) makes Claude move the conversation to its
background service, out of the herdr tab. If you really want that behavior, override it in the
model's `env`.

### Finding your shell commands automatically (discovery)

You may have wrapped your own commands in the shell, for example `claude-personal` for another
account, or `codex-pool` to rotate between a few setups. When the server starts, it opens your
interactive shell (`bash` or `zsh`) once and collects every function, alias and PATH command named
`claude`, `codex`, `claude-*` or `codex-*`. Each one automatically becomes a model with both
suffixes, for example `claude-personal+agent-tools` and `claude-personal+client-tools`.

A candidate is kept only if one of these is true:

- it is `claude` or `codex` itself, or you listed its name in `[discovery] include`;
- it is a function or alias whose body really runs a standalone `claude` / `codex`
  (`command claude` and `headroom wrap codex` count; something like `codex_flags_ref`, which just
  happens to contain "codex" in its name, does not).

Plain files on PATH named `claude-*` (such as a desktop app like `claude-desktop`) are dropped by
default, unless you list them in `include`. Anything in `exclude` is always dropped.

Launching works like `herdr pane run`: the server opens a tab, types `<command> <args>` into the
shell, and herdr recognizes on its own which CLI started. So your wrapper must pass `"$@"` along
(for example `command claude "$@"`); otherwise the `--session-id` the server adds gets lost on
the way.

While discovery is on, `[[models]]` does not replace the whole list: the same id overrides that
model, and a new id adds one. Discovery only looks at the shell once at startup, so restart
`serve` after adding a function. To see which models will be used, run
`herdr-openai-gateway check`.

Each Codex variant has its own `CODEX_HOME`, and the herdr integration must be installed in each
one. If one is missing, startup prints a warning naming it:

```bash
CODEX_HOME=~/.codex-personal herdr integration install codex
```

### Which model actually answers

The CLI decides, not this server. The CLI uses its own default model, or whatever you put in the
model's `args`. Claude switches profiles with `env.CLAUDE_CONFIG_DIR`, and Codex with
`env.CODEX_HOME`. To pin these, define your own model ids in `[[models]]`. If you set
`[discovery] enabled = false`, the old rule applies: only the four built-in ids, unless
`[[models]]` lists others.

## How conversations are matched

One client conversation maps to one agent tab in herdr. The server recognizes a conversation by a
"conversation key", checking these places in order and using the first one it finds:
`x-herdr-conversation`, `x-opencode-session-id`, `x-session-id`, `prompt_cache_key` in the body,
and finally `user`.

The key never includes the model, so one key always lands on one conversation. Switching model
mid-conversation only replaces the CLI behind it: the old tab closes and a new one opens, the whole
history is replayed, and the conversation id and the key stay the same.

If there is no key at all, it compares message history instead. If exactly one open conversation
with the same model and no key continues it, the request goes there; otherwise a new conversation
starts.

Only the **new** messages are typed to the agent each time. The server remembers how far it has
already sent and only adds what is missing. Non-streaming responses carry three headers,
`x-herdr-conversation`, `x-herdr-agent` and `x-herdr-tab`, so you can tell which tab a request
landed in.

If a request carries `x-opencode-session-id`, the server uses Linux's `/proc` to find that
opencode process on loopback. Once the process exits, its agent is shut down within about five
seconds. Running `opencode --continue` later replays the history and opens a new agent. When the
process cannot be identified (not Linux, a remote connection, or a shared
`opencode serve --service`), the agent follows the `idle_ttl_secs` rule instead.

Other rules:

- When a conversation sits idle longer than `conversations.idle_ttl_secs`, its agent is closed.
  The next request opens a new one and replays the history.
- If the backing CLI dies on its own, a 10-second **agent patrol** restarts it inside the **same
  pane** and recovers its own context with `--resume`／`resume`; only when the pane is gone too does a
  new tab open. After three restarts the patrol lets go of the conversation, closes that tab and stops
  tracking it, leaving it to the next request. A message sent while the restart is running gets 409
  `conversation_busy`; send it again once the CLI is up, while resending the same message is answered
  from the cache. The conversation key, the cwd, and the history stay put throughout.
- `max_agents_per_model` and `max_agents_total` limit how many agents are alive at once. When the
  limit is hit, the conversation unused the longest is asked to leave first.
- A conversation does one thing at a time. A new request that arrives before the previous turn
  ends gets 409 `conversation_busy`. The exception is when the previous client already hung up
  (for example, you pressed Esc in opencode). Then the new request takes over: the agent first
  gets an `esc`; Claude also gets a `ctrl+c`, because otherwise it puts the interrupted prompt
  back into its input box; and only then does the new prompt go in.
- A request that reuses a conversation key but contains no assistant messages at all is treated
  as a side request (such as generating a title or compacting the conversation) and gets HTTP 409
  right away. For the fix, see `small_model` in [Using it with opencode](#using-it-with-opencode).
  The key ignores the model, so pointing `small_model` at another model of this gateway keeps the
  title request on the same key: if it arrives first, it claims the conversation and the main
  request gets 409 for that attempt.

### Whose tab is it

Every pane behind the scenes wears a "name tag": a herdr metadata token called `oa_owner` that
says which server instance opened it. The tag is renewed every 30 seconds and expires 90 seconds
after the last renewal.

When leftover `oa:` tabs are cleaned up (at startup and on the 30-second tab sweep), any tab whose tag
names another live instance is left alone. So two servers can safely share one herdr session.

### After a server restart

Restarting the server (to upgrade it or change the config) does not lose your conversations. The
server keeps each conversation in a **state file**, like the log one shift leaves for the next: which
tab the conversation runs in, which messages the agent has already seen, and the CLI's session id. On
the next start it picks the conversations up from that log.

The state file lives at `$XDG_STATE_HOME/herdr-openai-gateway/state.json`, or at
`~/.local/state/herdr-openai-gateway/state.json` when `XDG_STATE_HOME` is unset. Set
`conversations.state_file` to put it somewhere else.

How a conversation is picked up depends on what survived:

- **The tab is still there**: a normal stop (Ctrl-C or SIGTERM) leaves the backing tabs open. The next
  start finds each one by its tab name (`oa:<model id>:<6 hex digits>`), and from then on only new
  messages are sent. If you never start the server again, close those tabs yourself.
- **herdr restarted too**: herdr keeps tab names but not name tags, so the server still finds the tab
  by name and puts a new tag on it. The CLI in the tab is gone; the patrol brings it back in the same
  pane with `--resume`／`resume`.
- **The tab is gone** (closed by hand, or the whole workspace is gone): the next request opens a new
  tab and runs `--resume`／`resume` with the saved session id, so the CLI gets its own context back and
  again only new messages are sent. If that fails, the request fails; send it again and a new agent
  starts with the history replayed.

A few things to watch out for:

- One state file serves one server: a second server pointed at the same file refuses to start. To run
  two servers, give each its own `conversations.state_file`. Once one of them has been down for 90
  seconds, the name tags on its tabs expire and the other server sweeps those tabs up as orphans.
- If the server dies in the middle of a turn, that message never made it into the state file. When
  the client resends it, the agent may get the same message twice.
- If you change the listen port, a Claude that was already running before the restart still sends its
  question hook to the old port.

## Using it with opencode

Add this to `opencode.json`:

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

A few easy traps:

- `name` is just the label in opencode's menu. Make it clear, so you can tell the mode at a glance.
- `timeout: false` is a must. When an agent is really working, a turn taking several minutes is
  normal.
- **Never** set `chunkTimeout`. Whether you set it to `false` or a number, opencode 2.0.28 quietly
  drops the whole provider, and `opencode models` shows no herdr models at all.
- `X-Herdr-Cwd` is optional. If set, it wins. If not, the server grabs the `Working directory`
  line from opencode's own system prompt (its `<env>` block), so the agent starts in the directory
  opencode is using. The full order is: `X-Herdr-Cwd` → the client's `Working directory` → the
  model's `cwd` → `defaults.cwd`. Whichever one ends up used, it must be under
  `defaults.allowed_cwd_roots`, or the request is blocked with 403 `cwd_not_allowed`, and the error
  tells you where that path came from.
- **Set `small_model` to a provider other than herdr.** opencode sends side requests such as title
  generation with the same session id. After a 500 ms merge wait they are blocked with HTTP 409.
  The only result is a missing title; the chat itself is fine.
- The first request after adding the provider is slower, because opencode installs the
  `@ai-sdk/openai-compatible` package first.

## When the agent shows a dialog

Claude Code and Codex often pop up dialogs: at startup they ask whether you trust this folder or
want to log in, and in the middle of work they ask whether they may run some command.

The server never clicks for you. It passes the dialog to the client so you choose yourself:

- If the client offers opencode's `question` tool, you see a `question` prompt carrying the
  agent's options.
- If not, you get a text message with numbered options. Just reply with the number you want.

Your answer becomes key presses sent to the agent's TUI, and then **the original turn carries
on**: the original message is not lost, no second agent is opened, and your answer is not typed in
as a new prompt.

A few special cases:

- If you leave a dialog unanswered longer than `herdr.blocked_timeout_ms`, it expires. An answer
  that arrives later gets an explanation asking you to resend your original message.
- A dialog that needs typed input (no options to choose) is passed to you as text naming the herdr
  tab. Answer it in that tab, and the conversation picks up once the agent is free.
- If your answer makes the agent quit (for example "No, exit"), that answer request gets 502
  `backing_agent_exited`, and the tab is closed.

Anyone who can reach this HTTP port can answer these dialogs, so protect the port as you would a
shell.

## Security

- By default it only binds `127.0.0.1:8787`, which means only your own machine can reach it. Leave
  it that way unless you are sure.
- Set `HERDR_OPENAI_GATEWAY_API_KEY` (or name another environment variable with
  `server.api_key_env`), and every request must carry `Authorization: Bearer <key>`. Leave it
  unset and no key is needed.
- The server checks that `Host` and `Origin` match the bound address. `server.allow_remote = true`
  relaxes that check and is meant for a proxy you run yourself. Turning on `allow_remote` requires
  an API key; without one the server refuses to start, even when bound only to loopback.
- `defaults.allowed_cwd_roots` decides which directories a request can point at, at most.
- Self-exec agents run commands and edit files in that directory with your local permissions.
  Anyone who can reach this HTTP port can command your agents, so protect the port as you would a
  shell.

## Config file

[`config.example.toml`](../config.example.toml) lists every option and its default, each with a
one-line explanation. Copy it to `~/.config/herdr-openai-gateway/config.toml` and edit away. Unknown
keys are rejected, so a typo fails loudly at startup instead of being silently skipped.

## API reference

| endpoint | notes |
| --- | --- |
| `GET /healthz` | whether herdr is reachable; 503 when it is not |
| `GET /v1/models` | model list; `created` is the server's start time |
| `GET /v1/models/{id}` | one model |
| `POST /v1/chat/completions` | streaming and non-streaming |

Supported request fields: `model`, `messages` (`system` / `developer` / `user` / `assistant` /
`tool`, plus `assistant.tool_calls`), `stream`, `stream_options.include_usage`, `tools`,
`tool_choice`, `parallel_tool_calls`, `user`, `prompt_cache_key`. Other sampling parameters are
accepted but have no effect. `n > 1` returns 400.

Stream order: role chunk → keepalive comments → content (at most 2 KiB per piece) → tool-call
chunks → finish chunk → usage chunk (if requested) → `[DONE]`. If something fails after the
stream has started, an error event is sent first, then `[DONE]`.

### Error codes

| code | HTTP | meaning |
| --- | --- | --- |
| `no_new_message` | 400 | the request brings nothing new |
| `prompt_too_large` | 400 | the new content to send exceeds the size limit |
| `unsupported_role` / `unsupported_parameter` | 400 | an unsupported role, or `n > 1` |
| `invalid_api_key` | 401 | no bearer key, or the wrong key |
| `cwd_not_allowed` | 403 | the cwd does not exist or is not under `allowed_cwd_roots`; the message tells you where the path came from (`X-Herdr-Cwd`, the client's `Working directory`, the model's `cwd`, or `defaults.cwd`) |
| `origin_not_allowed` | 403 | `Host` / `Origin` not allowed |
| `model_not_found` | 404 | no such model id |
| `conversation_busy` | 409 | a turn is still running in this conversation, or the patrol is replacing its agent |
| `conversation_side_request` | 409 | a side request sent to a chat conversation; set `small_model`. If a turn happens to be running when it arrives, you get `conversation_busy` instead |
| `too_many_backing_agents` | 429 | the agent limit is reached |
| `backing_agent_exited` | 502 | the agent died halfway; please resend |
| `agent_start_failed` / `agent_prompt_stalled` / `completion_not_observed` | 502 | the turn behind the scenes failed |
| `dialog_answer_failed` | 502 | the dialog cursor could not be moved onto the option you chose |
| `herdr_unavailable` | 503 | herdr is not running, or the socket path is wrong; the message names the socket. Once herdr is back it works again, with no need to restart herdr-openai-gateway |
| `agent_startup_blocked` | 503 | fallback case: the startup dialog could not be passed to you (the screen was unreadable), or it stayed stuck past `herdr.blocked_timeout_ms` |
| `turn_timeout` | 504 | the turn ran longer than `herdr.turn_timeout_ms` |

## Current limitations

- `usage` is always 0. The CLI's session log has no token counts, and an honest 0 beats a made-up
  number that would throw off the client's auto-compaction.
- No image input; images are replaced with `[image omitted]`.
- Claude Code and Codex only, and Unix only.
- If a turn produces no output for a long time, the client may cut it off with its own chunk
  timeout. The server sends an SSE keepalive comment every 10 seconds, but turns longer than a few
  minutes have not been tested for this.
- The reply is whatever the last message in the agent's session log says. So when the agent ends a
  turn with a question, or its answer is cut off halfway, it looks just like a normal reply.
