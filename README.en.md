[繁體中文](README.md) · [English](README.en.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja.md)

# herdr-openai-gateway

Lets tools that only speak the OpenAI API (such as opencode) put the Claude Code or Codex on your
own machine to work.

Think of it as a front desk: a tool hands it a question, it walks into [herdr](https://herdr.dev)
(a tool for managing terminals), types the question to Claude Code or Codex in a tab, waits for
the answer, and brings it back.

```text
opencode  ──question──▶  herdr-openai-gateway  ──types──▶  claude / codex in herdr
          ◀──answer────                        ◀──reads the session log──
```

The CLI runs in a terminal you can see. Switch over to watch any time, or step in yourself if
needed.

> **Read this first:** using it this way does not mean you comply with the provider's terms of
> service. Answering other tools' questions with a personal subscription may break those terms,
> so check them yourself.

## What you need

- herdr 0.9 or newer, already running
- Claude Code or Codex, signed in. For Codex, also run `herdr integration install codex`
- Rust (to build)
- Linux or macOS

## Install and run

```bash
cargo install --path .
herdr-openai-gateway check   # make sure herdr is reachable
herdr-openai-gateway serve   # start serving at http://127.0.0.1:8787/v1
```

## Try it

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -H 'X-Herdr-Cwd: /absolute/path/to/your/project' \
  -d '{"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}]}'
```

Switch to herdr and you will see a workspace called `openai-api`, with Claude Code working inside.

## Which model to pick

The suffix after the model name decides who does the hands-on work (editing files, running
commands):

| Suffix | Who does the work | What you see |
| --- | --- | --- |
| `+agent-tools` | the agent finishes the job itself inside herdr | only the final answer |
| `+client-tools` | the agent decides, opencode does the work | opencode's usual tool view and diffs |

Four are built in: `claude+agent-tools`, `claude+client-tools`, `codex+agent-tools`,
`codex+client-tools`. Commands you wrapped in your shell (such as `claude-personal`) show up in
the list automatically too.

## Hook up opencode

Add this to `opencode.json` (add the two Codex models the same way):

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

Three things to remember:

- `timeout: false` is a must; an agent often needs several minutes.
- Do not set `chunkTimeout`, or opencode 2.0.28 makes the whole provider disappear.
- Set `small_model` to a provider other than herdr, or title-generation requests get blocked (the
  chat itself is fine).

## Security

- By default it only listens on `127.0.0.1`, so only your own machine can reach it.
- To add a lock: set `HERDR_OPENAI_GATEWAY_API_KEY`, and every request must then carry
  `Authorization: Bearer <key>`.
- Anyone who can reach this port can make your agents run commands and edit files. Protect it the
  way you would protect a shell.

## Learn more

The [user guide](docs/guide.en.md) has the full details, including:

- startup flags and the config file
- model details, and how your shell commands are found automatically
- how conversations map to herdr tabs, and when idle ones are closed
- how to answer when the agent shows a dialog
- API fields and error codes
- current limitations

## License

Apache-2.0.
