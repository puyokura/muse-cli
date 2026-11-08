# muse

Meta Muse（`com.facebook.aura`、通称 Hatch）を CLI から操作する Rust 製ツール。
同じバイナリが `muse mcp` で MCP サーバーにもなる。

対象 APK: `com.facebook.aura_14.0.0.4.149-1061901406_minAPI29(arm64-v8a)(nodpi)_apkmirror.com.apk`

## ビルド

```
cd muse-mcp
cargo build --release
```

`target/release/muse.exe`（Windows）または `target/release/muse` が生成される。単体テストは `cargo test`。

## まず読むこと: Windows での文字化けと引用符

Windows PowerShell 5.1 には独立した2つの問題がある。どちらも実測で確認した。

**1. パイプすると非ASCIIが `?` になる。** `$OutputEncoding` の既定が us-ascii（コードページ20127）。

```
送信前: {"prompt":"日本語テストです"}
受信  : b'{"prompt":"????????"}\r\n'
```

**2. 二重引用符が落ちる。** 単一引用符で囲んでも、ネイティブプロセスに渡る前に消える。

```
'muse に渡る argv':  ["{method:/api/session/delete,session_id:x}"]
```

つまり `--json` に JSON を直接書く方法は、このシェルでは成立しない。そこで **`--set key=value`** を用意した。
引用符を一切含まないので、どのシェルでも壊れない。真偽値・整数・カンマ区切りの配列は自動で型付けする。

```
muse rpc POST /fs/stat --set path=MEMORY.md
muse rpc POST /api/session/delete --set session_id=xxxx --set method=/api/session/delete --confirm
muse goals list --set include_archived=true
muse fs search --set name=x --set extensions=md,txt
```

JSON を渡す手段は優先順に次のとおり。

| 方法 | 使える場面 |
|---|---|
| `--set key=value` | 常に。第一選択 |
| `--json-file PATH` | 常に。入れ子の深い JSON やファイルから |
| `--json JSON` | bash / cmd など引用符を保つシェル |
| `--json -` | 明示時のみ stdin を UTF-8 として読む |

`--json -` を使うときだけ次を設定する。

```powershell
$OutputEncoding = New-Object System.Text.UTF8Encoding($false)
```

## 認証

OTP は 2 段階。`restart` が発行する CSRF トークンと waterfall ID は状態ファイルに永続化され、
別プロセスで `--finish` を実行しても維持される。

```
muse login you@example.com JP     対話式
muse login --start you@example.com JP   送信のみ
muse login --finish 123456              確定
muse status
```

セッションは `%APPDATA%\muse-mcp\session.json`（Windows）、
`$XDG_CONFIG_HOME/muse-mcp/session.json`、`$HOME/.config/muse-mcp/session.json` の順で解決する。
`MUSE_MCP_STATE` か `--state PATH` で指定もできる。所有者のみ読める権限で保存する。

abra トークン（Stefi REST 用）とゲートウェイ資格情報は別物なので、状態ファイルでは
`abra_token` と `hatch_credential` に分けて保存する。混ぜると REST が 401 になる。

## サブチャット（スレッド）

`--thread <name>` を付けると、その名前専用のサブチャットで実行する。メインの会話を汚さない。

名前から UUID を決定論的に導出するので、同じ名前なら同じスレッドに継続する。
専用の作成APIは存在せず（`/api/session/create` 等は 404）、`/chat/stream` に
クライアント生成の `session_id` を渡すと新規スレッドが生まれる仕様を利用している。
作成時に `[name]` というタイトルへ自動で rename する。

```
muse thread new mcp-lab "このスレッドは実験用です"
muse chat --thread mcp-lab "テスト"
muse threads                一覧（--all で other も、--json で完全なJSON）
muse thread rename mcp-lab 新しい名前
muse thread archive mcp-lab
muse thread delete mcp-lab --confirm
```

未作成の名前を指定すると、勝手に作らずに作成コマンドを案内して止まる。
実験が意図しない場所に落ちないようにするため。

## VM のシステムファイル（エージェント経由）

ゲートウェイのファイルAPI（`/fs/*`）はワークスペースに閉じている。実測した拒否は次のとおり。

| 試行 | 応答 |
|---|---|
| 絶対パス `/etc/passwd` | `absolute paths are not allowed` |
| `..` | `path traversal is not allowed` |
| URLエンコードした traversal | `invalid path` |

`/api/exec` や `/api/terminal` は 404、`/api/tasks/shell` はルートはあるが
`matched route missing from endpoint ACL`。SSH operator はエンドースメント発行までは通るが
`<vm-id>.metaaivm.com` にAレコードが無く到達できない。

一方、エージェントは VM 内で **root のシェル** を持つ。実測で `id` は `uid=0(root)`、
`cat /etc/os-release` は実データ（Ubuntu 24.04.5 LTS / IMAGE_ID=hatch-image）を返した。
そこでシステムファイルはこの経路で読む。

```
muse exec "id; cat /etc/hostname" --thread mcp-lab
muse sys cat /etc/os-release --thread mcp-lab
muse sys ls /etc/ssh --thread mcp-lab
muse sys find /var/log --thread mcp-lab
muse sys raw "df -h" --thread mcp-lab
```

結果は必ず次の注記付きで返る。直接のファイル読み取りではないことを利用者が誤認しないようにする。

```json
{
  "transport": "agent-shell",
  "note": "executed by the Muse agent inside its VM (root shell); not a direct file API read"
}
```

`--thread` を省くとメインチャットで実行し、その旨を stderr に出す。

## ワークスペースのファイル（直接API）

```
muse fs ls [path]
muse fs stat <path>
muse fs cat <path> [--offset N] [--len N] [--base64]
muse fs get <remote> <local>
muse fs put <local> <remote>
muse fs mkdir <path> [--recursive]
muse fs mv <from> <to>
muse fs rm <path> [--recursive] [--confirm]
muse fs search <query> [--ext md,txt]
muse fs library [--preset recents|media|artifacts]
```

`fs cat` は `/fs/reads` の `offset` を使う。窓がUTF-8の文字境界に乗らない場合は
サーバーが `text` を返さないので、base64 と理由（`why_base64`）を返す。
`fs put` は 512 KiB チャンクで送り、複数チャンクは一時ファイル経由で rename する。
実測で 1,258,291 バイトの `put` → `get` がバイト一致した。

## その他

```
muse ping | model | status
muse history [--thread N] [--limit N]
muse cancel <session-id>
muse settings read
muse settings set-voice <voice-id>
muse goals list|get|create|update|delete
muse web <METHOD> /api/... [--json JSON] [--csrf]
muse rpc <METHOD> <path> [--set k=v]... [--json-file F] [--confirm]
muse tools
muse call <tool> [--json JSON]
muse mcp
```

## 新着の追跡（`history --follow`）

`/chat/subscribe` は購読としては存在するが、実測では応答が返るまで 60 秒以上かかった。
そのため `--follow` は履歴ポーリングで実装している。既存の最終 `seq` を起点に、
新しいイベントだけを JSON Lines で 1 行ずつ出力する。

```
muse history --follow                     追跡開始（Ctrl+C まで）
muse history --follow --until-idle 30     30秒無通信で自動終了
muse history --follow --count 5           5件で終了
muse history --follow --interval 5        ポーリング間隔（既定2秒）
muse history --follow --thread lab --until-idle 10
```

出力はイベント1件1行なので、そのまま `jq` などへ渡せる。
追跡開始・終了の通知は stderr に出るため、stdout は純粋なイベント列になる。

## バイナリ応答の保存（`muse raw`）

`/fs/export` は JSON ではなくバイナリを返す（実測では PDF）。`muse raw` はそうした
経路を取得してファイルへ書き、内容を判定して報告する。

```
muse raw /fs/export --method POST --json-file body.json --out export.bin
```

判定は先頭バイトで行う（zip / pdf / png / jpeg / text / unknown）。
サーバーが意図した型ではなく、実際のバイト列から判定した結果を返す。

## 認証の自動回復

ゲートウェイが 401（`hatchAdmissionInvalidated` / `session_invalid`）を返した場合、
保存済みクッキーで `auth_check` を再実行して abra トークンを再発行し、元の要求を1回だけ再試行する。
OTP の再入力は不要。回復に失敗したときは「`muse login` が必要」と明示して止まる。

## 応答待ちの仕組み

`/chat/stream` は送信の受付 ACK（`message_id` と `session_id`）だけを返す。本文は履歴から取る。
履歴にはストリーミング途中の断片も現れるため、次の2条件のどちらかを満たすまで待つ。

1. `status` が `completed` / `failed` / `error`
2. `status` が無い応答では、本文が2回連続で変化しないこと

当初は「`status` が無ければ確定」としていたため、フェンスだけの断片（` ``` `）を返す不具合があった。

## MCP モード

```
muse mcp
```

stdio の JSON-RPC。ツールは 34 個。

| ツール | 内容 |
|---|---|
| `muse_status` / `muse_login_start` / `muse_login_verify` / `muse_logout` | 認証 |
| `muse_ping` / `muse_model` | 疎通・モデル |
| `muse_sessions` / `muse_history` / `muse_ask` / `muse_cancel` | 会話 |
| `muse_threads` / `muse_thread` | サブチャットの一覧・作成・管理 |
| `muse_exec` / `muse_sys` | VM 内シェル（エージェント経由） |
| `muse_fs_list` / `muse_fs_stat` / `muse_fs_read` / `muse_fs_write` / `muse_fs_upload` / `muse_fs_delete` / `muse_fs_rename` | ワークスペース |
| `muse_fs_search` / `muse_fs_library` / `muse_fs_mkdir` / `muse_fs_download` / `muse_fs_put` / `muse_raw` | 検索・ライブラリ・ディレクトリ・転送・バイナリ保存 |
| `muse_goals` / `muse_session_action` / `muse_settings` | ゴール・セッション・設定 |
| `muse_account_get` / `muse_account_post` | `/hatch/*` REST |
| `muse_rpc` / `muse_web_api` | 汎用（破壊的操作は `confirm` 必須） |

dsh への登録は `.dsh/cordis.patch.yml`。

```yaml
- insert:
    - id: muse-mcp
      name: "@deepseek-ai/dsh-mcp-client"
      config:
        serverName: muse
        transport: stdio
        command: <リポジトリ>/muse-mcp/target/release/muse.exe
        args: ["mcp"]
```

## 実測した範囲

2026-11-07、実アカウントで確認した。

- Noise XX ハンドシェイク成功、`/api/ping` が `server_ts_ms` を返す
- `/model` は `auto/auto`、context window 350000
- サブチャット作成（`is_thread: true`）、同一名での継続、rename
- `exec` が `uid=0(root)` と `/etc/os-release` の実データを返す
- `sys ls /etc/ssh`、`sys cat /etc/os-release`
- 日本語プロンプトの往復（「こんにちは」「さくら」を取得）
- `fs put`/`fs get` の 1.2 MiB バイト一致、`fs cat --offset`、`fs search`、`fs library`
- `settings read` の 8 セクション
- 破壊的ガード（`rm`/`thread delete`/`rpc DELETE`）が `--confirm` 無しで停止すること
- MCP の `initialize` / `tools/list` / `tools/call`

## プラットフォーム

Windows と Linux の両方でビルド・実行を実測している。macOS はソースが同じ経路を通るが未検証。

| 項目 | Windows | Linux |
|---|---|---|
| ビルド | `cargo build --release` | 同左（Debian 13 / rustc 1.99.1 で実測） |
| テスト | 44 件 green | 44 件 green |
| 状態ファイル | `%APPDATA%\muse-mcp\session.json` | `$XDG_CONFIG_HOME/muse-mcp/session.json`、無ければ `$HOME/.config/...` |
| 状態の権限 | 既定の ACL | 0600（ファイル）と 0700（ディレクトリ） |
| 保存方式 | 一時ファイル→rename | 同左 |

状態ファイルは所有者だけが読める権限で、書き込みは一時ファイル経由の rename で行う。
中断してもセッションが半端な JSON で壊れない。

`MUSE_MCP_STATE` を指定した場合はそのパスをそのまま使う。`HOME` も `XDG_CONFIG_HOME` も無い
環境では、ホーム直下を汚さないよう作業ディレクトリの `.muse-mcp-session.json` に置く。

## 制約

- Muse の非公開プロトコルに依存する。上流の変更で壊れうる
- システムファイルはエージェントのシェル実行に依存する。1回につきエージェントのターンを1回消費し、
  直接HTTPより遅い
- `/fs/*` はワークスペース外に出られない。サーバー仕様であり迂回しない
- SSH operator は DNS 未解決で到達できない
- ACL で閉じている経路（`/api/tasks/shell`、`/api/navigation/tab-order`、`/api/connectors` POST 等）は迂回しない
- 承認（`/api/approvals*`）と権限設定（`/api/permissions/*`）はサーバーに存在しない
- confidential VM の attestation は復号のみで、完全な検証チェーンは実装していない
