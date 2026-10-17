[繁體中文](guide.md) · [English](guide.en.md) · [简体中文](guide.zh-CN.md) · [日本語](guide.ja.md)

# 使い方ガイド

[README](../README.ja.md) では始め方だけを説明しています。細かいことはすべてこのページにまとめました。必要なときに調べれば十分で、最初から全部読む必要はありません。

## 用語ミニ辞典

初めてなら、まずはこれだけ覚えておけば大丈夫です。

| 用語 | ひとことで言うと |
| --- | --- |
| herdr | AI のコーディング agent 向けに作られたターミナル管理ツールです。ビルにたとえると、workspace がフロア、tab が部屋、pane が部屋の中の机です。机 1 つにつき 1 つのプログラムを動かせます。 |
| agent CLI | ターミナルで動く AI アシスタントです。ここでは Claude Code（`claude`）と Codex（`codex`）を指します。 |
| OpenAI 互換 API | OpenAI が決めた HTTP インターフェースの形式です。多くのツールがこの形式を話すので、これをまねればそのままつなげられます。 |
| transcript | agent CLI が自動でディスクに書く会話ログ（JSONL ファイル）です。このサーバーはここから答えを読みます。 |
| turn | 1 往復のことです。あなたがメッセージを送り、agent が作業をして返事をするまでを 1 turn と数えます。 |

## 用意するもの

| 項目 | 説明 |
| --- | --- |
| herdr 0.9 以降、しかも**先に起動しておく** | `serve` は起動するとすぐ herdr のドアをノックします（socket に ping します）。返事がなければそのまま終了し、どの socket を探したかをエラーで教えてくれます。herdr server の protocol は 20 以降が必要です。より新しい protocol では警告が出るだけで、そのまま使えます。 |
| Claude Code か Codex、どちらか一方でも両方でも | いつもどおりの方法でインストールしてログインしておきます。 |
| herdr の codex integration **v8 以降**（Codex を使う場合のみ） | `herdr integration install codex` でインストールします。これがないとサーバーが Codex の session id を受け取れず、会話ログを見つけられません。 |
| Rust ツールチェーン | ビルドに使います。この crate はまだ crates.io にありません。 |
| Linux または macOS | 今のところ Windows には対応していません。 |

## 起動オプション

herdr につながらないとき、`serve` は役に立たないサービスを無理に立ち上げたりせず、すぐにエラーを出して
どの socket を試したかを教えてくれます。別の herdr セッションにつなぐには、`--herdr-session <NAME>`
または `--herdr-socket <PATH>` を付けます。

設定ファイルは `~/.config/herdr-openai-gateway/config.toml` です。あれば読み込み、なければ組み込みの
デフォルト値を使います。よく使うフラグは次のとおりです。

| フラグ | できること |
| --- | --- |
| `--config <PATH>` | 別の設定ファイルを読む |
| `--listen <ADDR>` | 待ち受けアドレスを変える（`server.listen` を上書き） |
| `--herdr-session <NAME>` / `--herdr-socket <PATH>` | 別の herdr セッションや socket につなぐ |
| `--log-requests` | リクエストのヘッダー（認証情報は伏せ字）と body の構造をログに残す。メッセージ本文は一切残さない |

## どの model を選ぶか

`/v1/models` に並ぶのは「いま動いている agent」ではなく、「注文できるメニュー」です。各項目には、
どのモードかを説明する非標準の `description` フィールドも付いています（opencode はこのフィールドを
読みません）。

注文しても、すぐには調理が始まりません。会話の最初のメッセージが届いて初めて、サーバーが herdr に tab
を開き、CLI を起動して、あなたの代わりに操作します。その後、クライアントが新しいセッションを始める
（たとえば opencode の `/new`）まで、その会話は同じ agent を使い続けます。

model 名の最後には必ずサフィックスが付きます。違いは、ツール（ファイルの編集やコマンドの実行など）を
**誰が実際に動かすか**です。

- `+agent-tools`：仕事を丸ごと agent に任せます。agent は自分の tab でファイルを編集し、shell を動かし、
  権限の確認もそこで行います。あなたが受け取るのは最後のメッセージだけです。
- `+client-tools`：agent は考えるだけで、手を動かすのは opencode です。agent が「このファイルを読みたい」
  と言うと、opencode が自分のツールで読み、自分の権限確認と diff 画面を出し、結果を agent に返します。

| id | モード | 誰が動かすか | クライアントに見えるもの |
| --- | --- | --- | --- |
| `claude+agent-tools` | self-exec | Claude Code が herdr の tab で全部やる：ファイル編集、shell の実行、権限の確認 | 最後のテキストだけ |
| `claude+client-tools` | bridge | Claude 自身のツールはすべてオフ（`--tools ""`）。opencode が自分の read/edit/bash/MCP ツールで実行し、結果を返す | テキストと `tool_calls` |
| `codex+agent-tools` | self-exec | Codex が herdr の tab で全部やる（上と同じ） | 最後のテキストだけ |
| `codex+client-tools` | bridge | Codex は読み取り専用の shell（`-s read-only -a never`：読み取り専用 sandbox、確認なし）を持っているが、bridge プロトコルで使わないよう指示される。動かすのは opencode | テキストと `tool_calls` |

### 2 つのサフィックスの選び方

| | `+agent-tools` | `+client-tools` |
| --- | --- | --- |
| 内部モード | `ModelMode::SelfExec` | `ModelMode::Bridge` |
| 誰が動かすか | agent（自分の herdr tab の中で） | クライアント（opencode） |
| クライアントに返ってくるもの | 最後のテキストだけ | テキストと `tool_calls` |
| opencode の `tool_call` の設定 | `false` | `true` |

- ターミナルでいつもやっているように agent に最後まで任せて、結果だけ見たいなら `+agent-tools` です。
  このモードはクライアントから届いた `tools` をまったく見ないので、opencode の画面にツール呼び出しや
  diff は出てきません。作業はすべて herdr の tab の中で進むので、見たければそちらに切り替えてください。
- opencode のいつものツール画面を使いたいなら `+client-tools` です。普通の model プロバイダーをつないだ
  ときと同じ感覚で使えます。agent が「どのツールを使いたいか」をテキストのプロトコルで伝える仕組みに
  頼っています。agent の返事の形式が崩れていたら 1 回だけやり直し、それでもだめならプレーンテキストと
  して返します。

Claude の `+agent-tools` は、`AskUserQuestion`（Claude があなたに質問したいときに使うツール）を
PreToolUse HTTP hook で処理します。そのおかげで opencode は選択肢ごとの説明を表示でき、自由入力・
複数選択・一度に複数の質問にも対応します。Claude Code の設定に `allowedHttpHookUrls` がある場合は、
`http://127.0.0.1:8787/internal/claude-hooks/pre-tool-use`（または待ち受けアドレスに合わせた URL）を
忘れずに追加してください。追加していない場合や、クライアントが `question` ツールを提供していない場合は、
herdr TUI のダイアログを中継するやり方に切り替わります（[agent がダイアログを出したとき](#agent-がダイアログを出したとき)を参照）。

裏で動く Claude agent は `CLAUDE_CODE_DISABLE_AGENT_VIEW=1` 付きで起動します。理由は、空の入力行で ← を
押す（または `/bg` と打つ）と、Claude が会話をバックグラウンドサービスに移してしまい、herdr の tab から
いなくなるからです。どうしてもその動きにしたい場合は、model の `env` で上書きできます。

### shell のコマンドを自動で見つける（discovery）

shell で自分用のコマンドを作っているかもしれません。たとえば別アカウント用の `claude-personal` や、
いくつかの設定を順番に使う `codex-pool` などです。サーバーは起動時にあなたの対話 shell（`bash` か
`zsh`）を一度だけ開き、`claude`、`codex`、`claude-*`、`codex-*` という名前の function・alias・PATH
上のコマンドを全部集めます。見つかったものは自動的に model になり、両方のサフィックスが付きます。
たとえば `claude-personal+agent-tools` と `claude-personal+client-tools` です。

見つかった候補は、次のどちらかに当てはまるときだけ残ります。

- `claude` や `codex` そのものであるか、`[discovery] include` に名前を書いてある。
- function か alias で、中身が本当に単独の `claude` / `codex` を実行している（`command claude` や
  `headroom wrap codex` は該当します。`codex_flags_ref` のように、名前にたまたま codex が入っている
  だけのものは該当しません）。

PATH 上の `claude-*` という名前の普通のファイル（`claude-desktop` のようなデスクトップアプリなど）は、
`include` に書かない限りデフォルトで外されます。`exclude` に書いたものは常に外されます。

起動の仕組みは `herdr pane run` と同じです。サーバーが tab を開いて shell に `<command> <args>` を打ち
込むと、どの CLI が起動したかを herdr が自分で見分けます。そのため、自作のコマンドは必ず `"$@"` を
そのまま渡してください（例：`command claude "$@"`）。渡さないと、サーバーが付けた `--session-id` が
途中で消えてしまいます。

discovery が有効なあいだ、`[[models]]` は一覧全体を置き換えません。同じ id ならその model を上書きし、
新しい id なら 1 つ追加します。discovery が shell を見るのは起動時の 1 回だけなので、function を追加
したら `serve` を再起動してください。最終的にどの model が使われるかは、`herdr-openai-gateway check`
で確認できます。

Codex のバリアントはそれぞれ自分の `CODEX_HOME` を持っていて、herdr integration もそれぞれに入れる必要が
あります。入れ忘れていると、起動時にどれが足りないかを警告で教えてくれます。

```bash
CODEX_HOME=~/.codex-personal herdr integration install codex
```

### 実際に答えるのはどのモデルか

決めるのはこのサーバーではなく CLI です。CLI は自分のデフォルトモデルか、model の `args` に書いた設定を
使います。Claude は `env.CLAUDE_CONFIG_DIR` で、Codex は `env.CODEX_HOME` でプロファイルを切り替えます。
固定したいときは、`[[models]]` に自分の model id を定義してください。`[discovery] enabled = false` に
すると以前のルールに戻り、`[[models]]` に別の指定がない限り、組み込みの 4 つの id だけになります。

## 会話の対応づけ

クライアントの会話 1 つが、herdr の agent tab 1 つに対応します。サーバーは「会話キー」で相手を見分けます。
次の場所を順に見て、最初に見つかったものを使います：`x-herdr-conversation`、`x-opencode-session-id`、
`x-session-id`、body の `prompt_cache_key`、最後に `user`。

キーがまったくない場合は、メッセージの履歴を見比べます。「同じ model で、キーなしで、まだ開いている」
会話のうち、続きとしてつながるものがちょうど 1 つだけあればそこへ送ります。そうでなければ新しい会話を
始めます。

agent に打ち込むのは、毎回**新しい**メッセージだけです。サーバーはどこまで送ったかを覚えていて、足りない
分だけを足します。非ストリームの応答には `x-herdr-conversation`、`x-herdr-agent`、`x-herdr-tab` の
3 つのヘッダーが付くので、リクエストがどの tab に届いたかがわかります。

リクエストに `x-opencode-session-id` が付いている場合、サーバーは Linux の `/proc` を使って loopback 上の
opencode プロセスを探します。そのプロセスが終了すると、およそ 5 秒以内に裏の agent も片付けます。あとで
`opencode --continue` を実行すると、履歴を再生して新しい agent を開きます。プロセスを特定できない場合
（Linux 以外、リモート接続、共有の `opencode serve --service`）は、agent は `idle_ttl_secs` のルールで
片付けられます。

そのほかのルール：

- 会話が `conversations.idle_ttl_secs` より長く放置されると、その agent は閉じられます。次のリクエストが
  来たときに新しく開き直し、履歴を再生します。
- `max_agents_per_model` と `max_agents_total` で、同時に動く agent の数を制限します。上限に達すると、
  いちばん長く使われていない会話から退場してもらいます。
- 1 つの会話が一度にこなす仕事は 1 つだけです。前の turn が終わる前に新しいリクエストが来ると、409
  `conversation_busy` が返ります。例外は、前のクライアントがすでに自分で切断している場合（opencode で
  Esc を押したときなど）です。このときは新しいリクエストが引き継ぎます。まず agent に `esc` を送り、
  Claude にはさらに `ctrl+c` を送ります（送らないと、中断されたプロンプトを入力欄に戻してしまうため）。
  そのあとで新しいプロンプトを入れます。
- 同じ会話キーを使っているのに assistant のメッセージが 1 つもないリクエストは、付随リクエスト（タイトル
  生成や会話の圧縮など）とみなされ、すぐに HTTP 409 が返ります。対処法は
  [opencode と組み合わせる](#opencode-と組み合わせる)の `small_model` を見てください。

### 背後の tab は誰のものか

裏で動く pane には、それぞれ「名札」が付いています。どのサーバーインスタンスが開いたかを書いた、
`oa_owner` という herdr metadata token です。名札は 30 秒ごとに更新され、最後の更新から 90 秒たつと
無効になります。

残った `oa:` tab を片付けるとき（起動時と、定期的な見回りのたび）、名札に別の生きているインスタンスの
名前があれば、その tab には手を出しません。なので、2 つのサーバーで 1 つの herdr セッションを安心して
共有できます。

## opencode と組み合わせる

`opencode.json` に次を追加します。

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

つまずきやすいポイント：

- `name` は opencode のメニューに出る名前にすぎません。わかりやすく書いておけば、どのモードかがひと目で
  わかります。
- `timeout: false` は必須です。agent が本気で作業していると、1 turn に数分かかるのは普通のことです。
- `chunkTimeout` は**絶対に**設定しないでください。`false` でも数値でも、設定すると opencode 2.0.28 は
  プロバイダー全体を黙って捨ててしまい、`opencode models` に herdr の model が 1 つも出なくなります。
- `X-Herdr-Cwd` は省略できます。設定すればそれが優先されます。設定しない場合、サーバーは opencode 自身の
  system prompt（その中の `<env>` ブロック）から `Working directory` の行を拾い、opencode が使っている
  ディレクトリで agent を動かします。優先順位は `X-Herdr-Cwd` → クライアントの `Working directory` →
  model の `cwd` → `defaults.cwd` です。最終的にどれが使われても、`defaults.allowed_cwd_roots` の下に
  なければなりません。外れていればリクエストは 403 `cwd_not_allowed` で止められ、そのパスがどこから
  来たかをエラーが教えてくれます。
- **`small_model` には herdr 以外のプロバイダーを設定してください。** opencode はタイトル生成などの付随
  リクエストを同じ session id で送ってきます。これらは 500 ms のまとめ待ちのあと HTTP 409 で止められます。
  結果はタイトルが作られないだけで、チャット自体には影響ありません。
- プロバイダーを追加した直後の最初のリクエストは遅めです。opencode が先に `@ai-sdk/openai-compatible`
  パッケージをインストールするためです。

## agent がダイアログを出したとき

Claude Code や Codex は、よくダイアログを出します。起動時には「このフォルダーを信頼するか」「ログインするか」
を聞き、作業の途中では「このコマンドを実行してよいか」を聞いてきます。

サーバーが勝手にボタンを押すことは絶対にありません。ダイアログをクライアントに回して、あなた自身に選んで
もらいます。

- クライアントが opencode の `question` ツールを提供していれば、agent の選択肢を載せた `question` の
  質問が表示されます。
- 提供していなければ、番号付きの選択肢を並べたテキストメッセージが届きます。選びたい番号を返信するだけで
  大丈夫です。

あなたの答えはキー入力になって agent の TUI に送られ、**元の turn がそのまま続きます**。元のメッセージは
消えませんし、2 つ目の agent が開くこともなく、答えが新しいプロンプトとして打ち込まれることもありません。

いくつかの特別なケース：

- ダイアログを `herdr.blocked_timeout_ms` より長く放っておくと期限切れになります。そのあとで届いた答えには、
  元のメッセージを送り直すよう案内する返事が返ります。
- 文字の入力が必要なダイアログ（選べる選択肢がないもの）は、どの herdr tab かを書いたテキストとして届きます。
  その tab で答えれば、agent の手が空いたところで会話が続きます。
- 選んだ答えで agent がそのまま終了した場合（たとえば "No, exit"）、その答えのリクエストには 502
  `backing_agent_exited` が返り、tab も閉じられます。

この HTTP ポートにつながる人なら誰でもこれらのダイアログに答えられます。ポートは shell と同じつもりで
守ってください。

## セキュリティ

- デフォルトでは `127.0.0.1:8787` にだけバインドします。つまり、つながるのはあなた自身のマシンだけです。
  自信がなければ変えないでください。
- `HERDR_OPENAI_GATEWAY_API_KEY` を設定すると（または `server.api_key_env` で別の環境変数名を指定すると）、
  すべてのリクエストに `Authorization: Bearer <key>` が必要になります。設定しなければキーは不要です。
- サーバーは `Host` と `Origin` がバインドしたアドレスと合っているかを確認します。
  `server.allow_remote = true` はこの確認をゆるめるもので、自分で運用する proxy のためのものです。
  `allow_remote` をオンにするなら API キーは必須です。キーがないと、loopback にだけバインドしていても
  サーバーは起動を拒否します。
- `defaults.allowed_cwd_roots` で、リクエストが指定できるディレクトリの範囲を決めます。
- self-exec の agent は、あなたのローカル権限でそのディレクトリのコマンドを実行し、ファイルを編集します。
  この HTTP ポートにつながる人は誰でもあなたの agent に指示を出せるので、ポートは shell と同じつもりで
  守ってください。

## 設定ファイル

[`config.example.toml`](../config.example.toml) に、すべてのオプションとデフォルト値が 1 行の説明付きで
載っています。`~/.config/herdr-openai-gateway/config.toml` にコピーして編集すれば OK です。知らないキーは
拒否されるので、タイプミスは黙って無視されず、起動時にきちんとエラーになります。

## API リファレンス

| endpoint | 説明 |
| --- | --- |
| `GET /healthz` | herdr につながるか。つながらなければ 503 |
| `GET /v1/models` | model 一覧。`created` はサーバーの起動時刻 |
| `GET /v1/models/{id}` | 1 つの model |
| `POST /v1/chat/completions` | ストリームにも非ストリームにも対応 |

対応しているリクエストフィールド：`model`、`messages`（`system` / `developer` / `user` / `assistant` /
`tool` と `assistant.tool_calls`）、`stream`、`stream_options.include_usage`、`tools`、`tool_choice`、
`parallel_tool_calls`、`user`、`prompt_cache_key`。その他のサンプリングパラメーターは受け取りますが、
効果はありません。`n > 1` は 400 を返します。

ストリームの順番：role chunk → keepalive コメント → 本文（1 つあたり最大 2 KiB）→ tool-call chunk →
finish chunk →（求められていれば）usage chunk → `[DONE]`。ストリームが始まってから失敗した場合は、先に
error イベントを送り、そのあと `[DONE]` を送ります。

### エラーコード

| code | HTTP | 意味 |
| --- | --- | --- |
| `no_new_message` | 400 | リクエストに新しい内容が何もない |
| `prompt_too_large` | 400 | 送ろうとしている新しい内容がサイズの上限を超えている |
| `unsupported_role` / `unsupported_parameter` | 400 | 対応していない role、または `n > 1` |
| `invalid_api_key` | 401 | bearer キーがない、またはキーが違う |
| `cwd_not_allowed` | 403 | cwd が存在しないか、`allowed_cwd_roots` の下にない。パスがどこから来たか（`X-Herdr-Cwd`、クライアントの `Working directory`、model の `cwd`、`defaults.cwd`）をメッセージが教えてくれる |
| `origin_not_allowed` | 403 | `Host` / `Origin` が許可されていない |
| `model_not_found` | 404 | その model id はない |
| `conversation_busy` | 409 | この会話でまだ turn が動いている |
| `conversation_side_request` | 409 | チャットの会話に付随リクエストが送られた。`small_model` を設定してください。届いたときにちょうど turn が動いていれば、代わりに `conversation_busy` が返る |
| `too_many_backing_agents` | 429 | agent の数が上限に達した |
| `backing_agent_exited` | 502 | agent が途中で止まった。送り直してください |
| `agent_start_failed` / `agent_prompt_stalled` / `completion_not_observed` | 502 | 裏の turn が失敗した |
| `dialog_answer_failed` | 502 | ダイアログのカーソルを選んだ選択肢に動かせなかった |
| `herdr_unavailable` | 503 | herdr が起動していないか、socket のパスが違う。どの socket かはメッセージに書かれている。herdr が戻れば普通に使えるようになり、herdr-openai-gateway の再起動は不要 |
| `agent_startup_blocked` | 503 | 予備の経路：起動時のダイアログを回せなかった（画面を読めなかった）、または `herdr.blocked_timeout_ms` を超えて止まったまま |
| `turn_timeout` | 504 | turn が `herdr.turn_timeout_ms` より長くかかった |

## 現在の制限

- `usage` は常に 0 です。CLI の会話ログにはトークン数が載っていません。でたらめな数字でクライアントの
  自動圧縮を惑わせるより、正直に 0 を返すほうがましです。
- 画像の入力には対応していません。画像は `[image omitted]` に置き換わります。
- 対応しているのは Claude Code と Codex だけで、動くのは Unix だけです。
- turn が長いあいだ何も出力しないと、クライアントが独自の chunk タイムアウトで切ってしまうことがあります。
  サーバーは 10 秒ごとに SSE の keepalive コメントを送りますが、数分を超える turn ではまだ試していません。
- 返事の中身は、agent の会話ログにある最後のメッセージそのものです。なので、agent が質問で turn を
  終えたときや、答えが途中で切れたときも、普通の返事と同じように見えます。
