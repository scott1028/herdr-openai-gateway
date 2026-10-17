[繁體中文](README.md) · [English](README.en.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja.md)

# herdr-openai-gateway

OpenAI API しか話せないツール（opencode など）から、あなたのマシンにある Claude Code や Codex に
仕事を頼めるようにします。

受付窓口のようなものだと考えてください。ツールから質問を受け取ると、[herdr](https://herdr.dev)
（ターミナルを管理するツール）の中に入り、タブで Claude Code や Codex に質問を打ち込みます。返事が
出たら、それを持ち帰ります。

```text
opencode  ──質問──▶  herdr-openai-gateway  ──入力──▶  herdr の中の claude / codex
          ◀──答え──                        ◀──会話ログを読む──
```

CLI は見えるターミナルで動いています。いつでも切り替えて様子を見られますし、必要なら自分で手を
出すこともできます。

> **最初にお読みください：** この使い方がプロバイダーの利用規約に沿っているとは限りません。個人向け
> サブスクリプションで他のツールの質問に答えると規約違反になるおそれがあるので、ご自身で確認してください。

## 用意するもの

- herdr 0.9 以降（先に起動しておく）
- Claude Code か Codex（ログイン済み）。Codex を使うなら `herdr integration install codex` も実行
- Rust（ビルド用）
- Linux または macOS

## インストールして起動

```bash
cargo install --path .
herdr-openai-gateway check   # herdr につながるか確認
herdr-openai-gateway serve   # サービス開始：http://127.0.0.1:8787/v1
```

## 試してみる

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -H 'X-Herdr-Cwd: /absolute/path/to/your/project' \
  -d '{"model": "claude+agent-tools", "messages": [{"role": "user", "content": "hello"}]}'
```

herdr に切り替えると `openai-api` という workspace があり、その中で Claude Code が作業しています。

## どの model を選ぶか

model 名の後ろのサフィックスで、誰が実際に手を動かすか（ファイル編集やコマンド実行）が決まります。

| サフィックス | 誰が動かすか | 見えるもの |
| --- | --- | --- |
| `+agent-tools` | agent が herdr の中で最後までやる | 最後の答えだけ |
| `+client-tools` | agent が考え、opencode が動かす | opencode のいつものツール画面と diff |

組み込みは 4 つです：`claude+agent-tools`、`claude+client-tools`、`codex+agent-tools`、
`codex+client-tools`。shell で自作したコマンド（`claude-personal` など）も自動的に一覧に出てきます。

## opencode につなぐ

`opencode.json` に次を追加します（Codex の 2 つの model も同じ形で追加します）。

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

必ず覚えておきたい 3 つのこと：

- `timeout: false` は必須です。agent の作業には数分かかることがよくあります。
- `chunkTimeout` は設定しないでください。設定すると opencode 2.0.28 ではプロバイダーごと消えてしまいます。
- `small_model` には herdr 以外のプロバイダーを設定してください。そうしないとタイトル生成のリクエストが
  止められます（チャット自体には影響ありません）。

## セキュリティ

- デフォルトでは `127.0.0.1` でしか待ち受けないので、つながるのはあなたのマシンだけです。
- 鍵をかけたいときは `HERDR_OPENAI_GATEWAY_API_KEY` を設定します。以後、すべてのリクエストに
  `Authorization: Bearer <key>` が必要になります。
- このポートにつながる人は、あなたの agent にコマンドを実行させたりファイルを編集させたりできます。
  shell と同じように守ってください。

## もっと知りたいとき

[使い方ガイド](docs/guide.ja.md)に詳しい説明があります。

- 起動フラグと設定ファイル
- model の詳細と、shell のコマンドを自動で見つける仕組み
- 会話と herdr の tab の対応、アイドル状態の会話がいつ片付けられるか
- agent がダイアログを出したときの答え方
- API のフィールドとエラーコード
- 現在の制限

## ライセンス

Apache-2.0。
