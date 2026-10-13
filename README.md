# herdr-openai-server

An OpenAI-compatible HTTP API backed by the agent CLIs [herdr](https://herdr.dev) already
manages. Point a tool that speaks the OpenAI chat API — opencode, or anything else — at
this server, and each request is answered by a real Claude Code or Codex session running in
a herdr pane, in your project directory, with your own local login.

```text
opencode / other OpenAI client
    │  POST /v1/chat/completions  (stream)
    ▼
herdr-openai-server ────── herdr socket API (NDJSON over a Unix socket)
    │                        agent.start / agent.prompt / agent.wait / agent.read
    ▼
herdr workspace "openai-api"
    │  one tab per client conversation
    ▼
claude / codex TUI ── writes its own session JSONL to ~/.claude or ~/.codex
    │
    ▼
herdr-openai-server reads the final reply back out of that JSONL
```

Nothing is intercepted: the CLI runs normally in a terminal you can watch and touch. The
request is typed into it, the answer is read from the transcript file the CLI already
writes. No proxying of provider traffic, no reuse of OAuth tokens, no screen scraping.

## Read this first

Driving the official CLI through herdr does **not** make this compliant with a provider's
terms of service. Using a consumer subscription plan to serve requests for other tools may
violate those terms; check them yourself. This project has no anti-detection features and
will not add any.

## Requirements

- herdr 0.9 or newer, **already running** with a session you can attach to: `serve` pings
  the socket at startup and exits with an error naming it when herdr is unreachable. The
  socket client accepts server protocol 20 and up (newer servers only warn).
- Claude Code and/or Codex CLI, installed and signed in the way you use day to day.
- For Codex, the herdr codex integration **v8 or newer**, which is what reports the session
  id this bridge needs: `herdr integration install codex`.
- A Rust toolchain to build (the crate is not published to crates.io yet).
- Linux and macOS. Windows is not supported.

## Install and run

```bash
cd herdr-openai-server
cargo install --path .      # or: cargo run -- serve

herdr-openai-server check   # ping herdr, print socket + protocol
herdr-openai-server serve   # http://127.0.0.1:8787/v1
```

Start herdr before `serve`: it pings the herdr socket at startup and, when herdr is not
reachable, exits with an error naming the socket it tried rather than serving a dead
bridge. Point it at another session with `--herdr-session` or `--herdr-socket`.

`serve` reads `~/.config/herdr-openai-server/config.toml` when it exists; with no config file at
all the built-in defaults apply. Useful flags: `--config <PATH>`, `--listen <ADDR>`,
`--herdr-session <NAME>` or `--herdr-socket <PATH>`, `--log-requests`.

Then in another terminal, watch it work: the bridge creates a herdr workspace named
`openai-api` and one tab per client conversation.

## Models

`/v1/models` lists the agent types you can pick in `model` — it is not a list of running
agents. Each `/v1/models` entry includes a non-standard `description` that explains its
mode; opencode does not read it. Nothing starts until the first message of a conversation:
the server then opens a herdr tab, starts that CLI in it, and drives it for you. The
conversation keeps that agent until the client starts a new session (opencode `/new`).

| id | mode | who runs the tools | what the client sees |
|---|---|---|---|
| `claude+agent-tools` | self-exec | Claude Code runs its own tools in the herdr tab — edits, shell, its own approval prompts | only the final text |
| `claude+client-tools` | bridge | the agent's own tools are off (Claude `--tools ""`); opencode runs its own read/edit/bash/MCP tools with its own permission prompts and diff view and sends the results back | text plus `tool_calls` |
| `codex+agent-tools` | self-exec | Codex CLI runs its own tools in the herdr tab, as above | only the final text |
| `codex+client-tools` | bridge | Codex keeps a read-only shell (`-s read-only -a never`: read-only sandbox, no approvals) and the bridge protocol tells it not to use it; opencode runs its own tools, as above | text plus `tool_calls` |

Self-exec ignores the client's `tools` completely, so opencode shows no tool calls and no
diffs; the work happens in the herdr tab, where you can watch it. Bridge is how a normal
model provider behaves in opencode.

Claude `+agent-tools` handles `AskUserQuestion` through a PreToolUse HTTP hook, so
opencode can show option descriptions and support free text, multiple selection, and
multiple questions. If Claude Code settings define `allowedHttpHookUrls`, add
`http://127.0.0.1:8787/internal/claude-hooks/pre-tool-use` (or the URL for your listen
address). Without that permission, or when the client does not offer the `question` tool,
the hook falls back to the existing herdr TUI dialog relay.
Backing Claude agents start with `CLAUDE_CODE_DISABLE_AGENT_VIEW=1`, because ← on an empty
prompt (or `/bg`) would move the conversation to Claude's background service and out of the
herdr tab; set it in a model's `env` to override.

The two suffixes side by side:

| | `+agent-tools` | `+client-tools` |
|---|---|---|
| internal mode | `ModelMode::SelfExec` | `ModelMode::Bridge` |
| who runs the tools | the agent, inside its herdr tab | the client (opencode) |
| what the client gets back | only the final text | text plus `tool_calls` |
| opencode `tool_call` | `false` | `true` |

Which one? Use `+agent-tools` when you want the agent to work as it does in its own TUI and
just read the answer; use `+client-tools` when you want opencode's normal tool UI. Bridge
relies on the agent following a text protocol; a malformed reply is retried once, then
returned as plain text.

By default the model list comes from **discovery**: at startup the server runs your
interactive shell (`bash` or `zsh`) and collects every function, alias and PATH command
named `claude`, `codex`, or `claude-*` / `codex-*` — so shell functions like
`claude-personal` or `codex-pool` show up as models automatically, each with its
`+client-tools` bridge twin (`claude-personal+agent-tools`,
`claude-personal+client-tools`, ...).

A candidate is kept when:

- it is `claude` or `codex` itself, or its name is listed in `[discovery] include`;
- it is a function or alias whose definition runs a standalone `claude` / `codex` token
  (`command claude`, `headroom wrap codex` count; `codex_flags_ref` does not);
- plain files on PATH named `claude-*` (e.g. a GUI app like `claude-desktop`) are dropped
  unless listed in `include`; anything in `exclude` is always dropped.

Launch works like `herdr pane run`: the server opens a tab, types
`<command> <args>` into its shell, and herdr detects the running CLI by itself. A variant
must pass `"$@"` through to the real CLI (e.g. `command claude "$@"`), otherwise the
`--session-id` the server passes is lost.

With discovery on, `[[models]]` entries no longer replace the built-in list: an entry with
the same id overrides that model, a new id adds one. Discovery reads the shell once at
startup — restart `serve` to pick up new shell functions. `herdr-openai-server check` prints the
model list it would use.

Each Codex variant keeps its own `CODEX_HOME`, and each one needs the herdr integration
installed separately, otherwise startup logs a warning naming it:

```bash
CODEX_HOME=~/.codex-personal herdr integration install codex
```

Which upstream model actually answers is decided by the CLI, not by this server: it uses the
CLI's default model, or whatever you pass in the model's `args`. For Claude, a profile is
selected with `env.CLAUDE_CONFIG_DIR`; for Codex, with `env.CODEX_HOME`. Define your own
model ids in `[[models]]` to pin these. With `[discovery] enabled = false` the old rule
applies: the four built-in ids unless `[[models]]` lists others.

## Conversations

One client conversation ↔ one backing agent tab. The conversation key is taken from the
first of: `x-herdr-conversation`, `x-opencode-session-id`, `x-session-id`, then the body's
`prompt_cache_key`, then `user`. With no key at all, the request is matched by its history:
if exactly one open keyless conversation of that model continues it, the request goes there,
otherwise it starts a new one.

Only the new messages are typed into the agent: the server tracks how much of the history it
has already delivered and sends the delta. Non-stream responses carry `x-herdr-conversation`,
`x-herdr-agent` and `x-herdr-tab` so you can see the mapping.

For requests with `x-opencode-session-id`, the server uses Linux `/proc` to identify the
loopback opencode process and closes its backing agent within about five seconds after that
process exits. A later `opencode --continue` replays history and starts a new agent. If the
process cannot be identified (non-Linux, remote, or shared `opencode serve --service`), the
agent remains subject to `idle_ttl_secs`.

- An idle conversation's agent is closed after `conversations.idle_ttl_secs`; the next
  request respawns it and replays the history.
- `max_agents_per_model` and `max_agents_total` cap live agents; over the cap the least
  recently used conversation is evicted.
- One turn at a time per conversation. A second request while a turn is in flight gets 409
  `conversation_busy` — unless the first client already hung up (opencode's Esc), in which
  case the new request supersedes it: the agent gets an `esc`, Claude also gets one `ctrl+c`
  because it otherwise puts the interrupted prompt back into its input box, and the new
  prompt goes in.
- A request that repeats a conversation key but carries no assistant messages is treated as
  a side request (title generation, compaction) and is rejected with HTTP 409.
  See `small_model` below.

## opencode

`opencode.json`:

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

- `name` is only a display label: it is what the opencode model picker shows, so the four
  ids explain themselves.
- `timeout: false` is required: a real agent turn can take many minutes.
- Do **not** set `chunkTimeout`. With it set, `false` or a number, opencode 2.0.28
  silently drops the whole provider: `opencode models` lists no herdr models at all.
- `X-Herdr-Cwd` is optional. When it is set it wins; without it the server takes the
  `Working directory` line from opencode's own system prompt (its `<env>` block), so the
  agent starts in the directory opencode is working in. Full precedence: `X-Herdr-Cwd` →
  the client's `Working directory` → the model's `cwd` → `defaults.cwd`, and every one of
  them must be inside `defaults.allowed_cwd_roots` or the request fails with 403
  `cwd_not_allowed`, which names the source the path came from.
- **Set `small_model` to a provider that is not herdr.** opencode sends title-generation
  and similar side requests on the same session id; they are rejected with HTTP 409 after a
  500 ms coalescing wait, which shows up as a failed title, not a broken chat.
- The first request after adding the provider can take a while: opencode installs the
  `@ai-sdk/openai-compatible` package on first use.

## Dialogs

Claude Code and Codex show dialogs — folder trust or login at startup, permission
prompts during a turn. The server never picks an option itself. The dialog is relayed
to you in the client instead: when your client offers opencode's `question` tool the
relay is a `question` call carrying the agent's options; otherwise it is a numbered
text reply you answer with the number of your choice. The answer is pressed into the
agent's TUI as keys and the **same turn continues** — the original message is never
lost, no second agent is started, and the answer is never typed into the agent as a
prompt.

- An unanswered dialog expires after `herdr.blocked_timeout_ms`; a late answer then
  gets an explanatory reply and you resend the message.
- Dialogs that need typed input (no selectable options) are relayed as text pointing
  at the herdr tab: answer them there and the conversation continues once the agent
  is free.
- If the agent exits after your answer (for example "No, exit"), the answer request
  gets 502 `backing_agent_exited` and the tab is closed.

Anyone who can reach this HTTP port can answer these dialogs — treat the port like
a shell.

## Security

- Binds `127.0.0.1:8787` by default. Do not change that lightly.
- Set `HERDR_OPENAI_SERVER_API_KEY` (or `server.api_key_env` to another name) to require
  `Authorization: Bearer <key>`. Unset means no key.
- `Host` and `Origin` are checked against the bind address; `server.allow_remote = true`
  relaxes that for a proxy you run yourself.
- `defaults.allowed_cwd_roots` limits which directories a request may ask for.
- Self-exec agents run commands and edit files in that directory with your local
  permissions. Anyone who can reach this HTTP port can drive your agents: treat the port
  like a shell.

## Configuration

Every option, its default, and a one-line comment: [`config.example.toml`](config.example.toml).
Copy it to `~/.config/herdr-openai-server/config.toml` and edit. Unknown keys are rejected, so a
typo fails at startup rather than being ignored.

## API

| endpoint | notes |
|---|---|
| `GET /healthz` | herdr reachability; 503 when herdr is unreachable |
| `GET /v1/models` | model list, `created` is the server start time |
| `GET /v1/models/{id}` | one model |
| `POST /v1/chat/completions` | stream and non-stream |

Supported request fields: `model`, `messages` (`system`/`developer`/`user`/`assistant`/`tool`,
`assistant.tool_calls`), `stream`, `stream_options.include_usage`, `tools`, `tool_choice`,
`parallel_tool_calls`, `user`, `prompt_cache_key`. Other sampling parameters are accepted and
ignored. `n > 1` is a 400.

Stream order: role chunk → keepalive comments → content in ≤ 2 KiB pieces → tool-call chunks
→ finish chunk → optional usage chunk → `[DONE]`. An error after the stream has started
arrives as an error event followed by `[DONE]`.

### Error codes

| code | HTTP | meaning |
|---|---|---|
| `no_new_message` | 400 | the request adds nothing to the conversation |
| `prompt_too_large` | 400 | the delta exceeds the prompt limit |
| `unsupported_role` / `unsupported_parameter` | 400 | bad role, or `n > 1` |
| `invalid_api_key` | 401 | missing or wrong bearer key |
| `cwd_not_allowed` | 403 | the cwd does not exist or is outside `allowed_cwd_roots`; the message names where the path came from (`X-Herdr-Cwd`, the client's `Working directory`, the model `cwd`, or `defaults.cwd`) |
| `origin_not_allowed` | 403 | `Host`/`Origin` not permitted |
| `model_not_found` | 404 | unknown model id |
| `conversation_busy` | 409 | a turn is in flight for this conversation |
| `conversation_side_request` | 409 | side request on a chat conversation; set `small_model`. One that arrives while a turn is in flight reports `conversation_busy` instead |
| `too_many_backing_agents` | 429 | agent caps reached |
| `backing_agent_exited` | 502 | the agent died mid-turn; resend |
| `agent_start_failed` / `agent_prompt_stalled` / `completion_not_observed` | 502 | the backing turn failed |
| `herdr_unavailable` | 503 | herdr is not running or the socket path is wrong; the message names the socket. Requests work again once herdr is back — no need to restart herdr-openai-server |
| `agent_startup_blocked` | 503 | fallback only: the startup dialog could not be relayed (unreadable screen) or stayed blocked past `herdr.blocked_timeout_ms` |
| `dialog_answer_failed` | 502 | the agent's dialog cursor could not be moved onto the chosen option |
| `turn_timeout` | 504 | the turn exceeded `herdr.turn_timeout_ms` |

## Limitations

- `usage` is always zero. The CLIs do not report token counts back through their
  transcripts, and faking them would drive a client's auto-compaction with garbage.
- No image input: image parts become `[image omitted]`.
- Claude Code and Codex only. Unix only.
- A client may still apply its own chunk timeout to a very long silent turn. This server
  sends SSE keepalive comments every 10 seconds, but that has not been verified against
  turns longer than a few minutes.
- The reply is whatever the agent's own transcript records as its final message, so an agent
  that ends a turn with a question, or with an interrupted answer, reads as a normal reply.
