# muse-cli

Muse をターミナルから操作する Rust 製ツール。同じバイナリが `muse mcp` で
MCP サーバーにもなる。

## ライセンス

AGPL-3.0-or-later。全文は `LICENSE`。

本プロジェクトは非公開プロトコルを実通信の観測から再構成した独立実装であり、
Meta による配布物ではない。依存クレートはいずれも AGPLv3 と互換
（MIT / Apache-2.0 / BSD-3-Clause）で、内訳は
`cargo tree --format "{p} {l}"` で確認できる。

## ビルド

```
cargo build --release
```

`target/release/muse` が生成される。単体テストは `cargo test`。

## 動作確認（`muse doctor`）

端末を開かずに、どこが壊れているかを層ごとに答える。

```
muse doctor            認証と状態ファイルだけ（ネットワーク不要）
muse doctor --live     ゲートウェイまで含めて全部
muse doctor --live --json
```

```
state     ok    6419 bytes at ~/.config/muse-mcp/session.json, mode 0600
device    ok    present (4657ee48…)
auth      ok    validated, viewer 1352809671259466
gateway   ok    ping ok, server_ts_ms 1794743037162
model     ok    auto/auto (1 available)
sessions  ok    44 conversations listed
```

`fail` が1つでもあれば終了コードは 1。`warn` と `skip` は 0 のまま。
`auth` が通って `gateway` が落ちる状態は「ゲートウェイだけ死んでいる」であり、
`muse status` だけでは見分けられない。そこを分けるためにこのコマンドがある。

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

## クオータ（`muse quota`）

`/hatch/subscription` を読んで、使用率を**分母とリセット時刻つきで**出す。
％だけでは「何の64％か、いつ戻るのか」が分からない。

```
$ muse quota
tier            Muse Free (HATCH_FREE)
subscription    FREE paid=false status=Weekly limit resets on Nov 18
usage           64% of Free plan  state=METERED status=SUFFICIENT
resets_at       2026-11-18 23:14:17Z
topup           80,000,000 / 80,000,000  0% used (2B tokens left)  eligible=false
catalog         upgrade_eligible=true invite_reward=true cta=Upgrade
```

`--json` は生のペイロードを返す。`usage.percent_used` が実際の使用率で、
`usage.quota_status` が `SUFFICIENT` / `DEPLETED`、`usage.resets_at` が
リセットの Unix 秒。`topup_balance` は追加トークンの残量。
存在しない項目は `-` で出す（行を消すと「対象外」に見えてしまう）。

MCP は `muse_quota`。

## 使用量の実数（`muse usage`）

クオータの％にはトークン実数の対応が無い。`/hatch/subscription` が返すのは
`percent_used` だけで、`ViewerUsageJson` のフィールドは `state` /
`percent_used` / `quota_status` の3つ（APK の DEX で確認）。週次上限も API
には出てこない。

実数はアプリの `/status` コマンドの応答にだけ存在するので、そこから読む。

```
$ muse usage
model           Muse Spark
lifetime        in 1,127,046,357  out 9,239,512  (account, all sessions)
session         in 9,020,282  out 6,431  (this conversation)
subagents       in 0  out 0
aggregate       in 9,020,282  out 6,431  (session + subagents)
context         used 72,145 / 350,000  (21%)
available       277,855  (79%)
quota           64% of Muse Free  status SUFFICIENT  resets 2026-11-18 23:14:17Z
topup           80,000,000 / 80,000,000 credits  = 2,000,000,000 / 2,000,000,000 tokens (25x)
note            the weekly cap is not exposed, so the quota percentage has no token equivalent; ...
```

各グループの下に **クレジット換算**が付く。クレジットは API 自身が使う単位
（`topup_balance` / `credits_granted`）なので、そのまま比較できる。

```
lifetime        in 1,124,424,991  out 9,277,061  (account, all sessions)
  credits       in 44,976,999.64  out 371,082.44
context         used 83,872 / 350,000  (24%)
  credits       used 3,354.88 of 14,000.00
```

換算比は 25 tokens/credit。catalog の4パック
（20M→500M、80M→2B、200M→5B、480M→12B）すべてで一致する。
クレジットは**導出値**で、サーバが返した値ではない。測った値と混ぜないため、
`--json` は `counters`（実測）と `credits`（導出）を分け、`derived` に
`credits.*` を載せる。

MCP は `muse_usage`。

## スラッシュコマンド（`muse slash`）

アプリが文書化しているコマンドを、そのまま1ターンとして送る。
先頭の `/name` を解釈するのはゲートウェイで、こちらは正しい1行を送るだけ。

```
muse slash --list
muse slash /status
muse slash /usage on
muse slash /compact --thread <name>
```

| コマンド | 引数 | 動作 |
|---|---|---|
| `/status` | - | セッションの状態（トークン数、コンテキスト使用率） |
| `/compact` | - | 会話コンテキストを圧縮 |
| `/clear` | - | 次のメッセージから新しいコンテキスト |
| `/usage` | `on` \| `off` | 使用量表示の切り替え |
| `/stop` | - | 進行中の応答を停止 |
| `/latest` | - | 最新のトランスクリプトへ戻る |
| `/activity` | - | Activity パネルを開く |

`/usage` は引数が必須。引数なしで送るとサーバが根拠の無い HTTP 400 を返すので、
送る前に `on|off` を要求する。未登録の名前は警告を出してから送る（黙って別物に
置き換えない）。MCP は `muse_slash`。

## セッションを直接指定する（`--session`）

`--thread` は名前から導出したサブチャットを指す。Muse から渡されたセッション ID のように、
名前を持たない会話を読み書きするときは `--session <uuid>` を使う。会話を扱うコマンド
（`chat` / `relay` / `history` / `cancel` など）はすべてこれを受けける。

```
muse history --session eeb1c4d1-8927-84dd-8037-9a23d2e2b2f0 --limit 3
```

UUID でない値は拒否する。`--thread` と併用した場合は `--session` を優先する。

**未知のフラグはエラーになる。** 以前は `muse history --muse-session <id>` が
フラグを黙って捨てて primary の履歴を返していた。受け取った側からは「宛先の履歴」と
見分けが付かないため、送達確認に使うと嘘の安心を与える。現在はコマンドごとに
受け付けるフラグを宣言し、それ以外は名前を示して停止する。

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
muse chat --file <path>     添付をファイルとして送る（ワークスペースに保存される）
muse chat --image <path>    添付を画像として送る（インラインで展開される）
```

```
muse ping | model | status
muse history [--thread N] [--limit N]
muse cancel <session-id>
muse settings read
muse settings set-voice <voice-id>
muse goals list|get|create|update|delete
muse web <METHOD> /api/... [--json JSON] [--csrf]
muse relay <prompt> --muse-session <id> --dsh-session <id> --request-id <id>
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

## ファイルの送信

添付には2種類あり、ゲートウェイの扱いが違う。実測で区別がついた。

| 種別 | フラグ / 引数 | ゲートウェイの扱い | 上限 |
|---|---|---|---|
| 画像 | `--image` / `attachment_kind: "image"` | デコードしてエージェントが中身を見る。壊れた画像はターンごと失敗する | 3.5 MB |
| ファイル | `--file` / `attachment_kind: "file"` | `workspace/user/files/<name>` に保存し、本文に `[file:mime path]` として参照を置く | 10 MB |

```
muse chat --file report.pdf "この PDF の要約をして"
muse relay --file notes.md --muse-session <id> "この内容に沿って返信して"
```

MCP は `attachments` に絶対パスを並べ、`attachment_kind` で種別を選ぶ。既定は `image`。

`--image` と `--file` は同時に指定できない。混在させると、どちらの形式で出すかを
サーバが勝手に決めることになるため、エラーにして選ばせる。`--image` に画像以外を渡しても
止まる（`pass it with --file instead`）。

MIME は拡張子から決める。ゲートウェイは値そのものを検証しない（pdf / zip / mp4 / json で
通ることを実測）ので、未知の拡張子は `application/octet-stream` と正直に申告する。

上限は実測値。1.5MB・5MB・10MB は通り、20MB は 60 秒でタイムアウトした。20MB は
サイズ上限ではなく、単一リクエストの転送時間が先に尽きる。

保存先はワークスペースの `workspace/user/files/`。ゲートウェイが付けるパスは
`workspace/user/files/<name>` で、`fs stat` には `/workspace/user/files/<name>` を渡す。

## 承認（`muse approvals`）

エージェントがツールを実行する前に、外への通信などを承認で止める仕組みがある。
この承認は**デーモンではなく sentinel サービスに載っている**。実測では `/approvals` 系は
デーモンで全て 404、サービス1で 200 だった。`muse rpc` はデーモン固定なので、
承認に届くには `muse service 1 ...` か `muse approvals` を使う。

```
muse approvals                     一覧（pending と recent）
muse approvals --json              全項目をそのまま
muse approvals show <approval-id>  1件の詳細
muse approvals decide <id> allow_once
muse approvals decide <id> deny
muse approvals decide <id> allow_always --confirm
```

一覧は1行1件で、id・状態・宛先・理由を出す。

```
recent  5c0b8bab-... timed_out  POST ruxa.xsns.jp/api/users/show egress approval request expired
```

決定に使える値は `allow_once` / `allow_session` / `allow_today` / `allow_always` /
`auto_allow` / `deny` / `deny_always` の7つ。実際に送る JSON は
`{"decisions":[{"approval_id":"...","decision":"deny"}]}` で、フィールド名は snake_case で
なければならない（camelCase は `missing field approval_id` で 400 になる）。

`allow_once` と `deny` 以外は、待っている要求の外へ効力が及ぶ。そのため `--confirm`
（MCP では `confirm: true`）を要求する。無い場合は送信前に止まる。

MCP では `muse_approvals` ツールが同じことをする（action=list/show/decide）。

## 遅いツールが他の呼び出しを止めない

MCP のツール呼び出しは1本の読み取りループから切り離して実行する。以前は要求を順番に
処理していたため、`muse_relay` や `muse_exec` のような長いターンが1つ走るだけで、
後ろに並んだ `muse_ping` も `muse_status` も**開始されないまま**クライアントの
タイムアウトに達した。呼び出し側には「そのツールが遅い」と「まだ動いていない」の区別が
付かないため、全ツールが死んでいるように見えた。

現在は `tools/call` をタスクとして起動し、応答は id で対応付けて書き出す。書き込みは
1つのミューテックスで直列化するので、1つの JSON-RPC メッセージが行の途中で混ざることはない。

実測（`sleep 6` の `muse_exec` の後ろに `muse_ping` を並べた場合）:

| | 修正前 | 修正後 |
|---|---|---|
| 遅い呼び出し | 8.4 秒 | 8.3 秒 |
| 後ろの ping | **8.6 秒（ブロック）** | **1.1 秒** |

長い待ちはクライアント側のタイムアウトで切られる前に、サーバー側が応答を返せるようになった。

## 切断からの復帰

ゲートウェイとの Noise ソケットは落ちる。落ちた原因は接続オブジェクトに記録され、
以降の要求はすべて `Hatch connection is closed` で失敗する。

以前はこの死んだハンドルをキャッシュから出し続けたため、**一度切れるとプロセスが
終わるまで全ツールが失敗した**。`muse_status` だけが通るのは、これがゲートウェイを
経由せずトークン検証 API を直接叩くためで、「生きている」証拠にならない。

現在は3段で復帰する。

1. `connect` は再利用前に閉鎖理由を見る。記録があれば捨てて新しく張る
2. RPC は死んだソケットのエラーを受けたら1回だけ張り直す（上限あり、無限再試行にしない）
3. WebSocket 接続は全候補を1巡したあと2秒待って、もう1巡する

3 は実測に基づく。同じ共有 ingress が一度は接続を拒否し、5秒後に受け入れた例があった。
呼び出し側には「一過性の失敗」と「VM が落ちている」を区別する手段が無いため、
1回だけやり直す。

復旧の判定は `muse_ping` で行う。`muse_status` はゲートウェイを通らないので使えない。

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

stdio の JSON-RPC。ツールは 36 個。

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

## 解析したアプリ版

2026-11-18 に 16.0.0.11.159（versionCode 1062101792）の DEX を 14.0.0.4.149 と全数比較した。
クライアントのプロトコルに影響する変更は無く、**このバージョンで接続と全機能を実測確認済み**。

不変だったもの:

- `X-API-Version: 1.0.0`（アプリ内 4 箇所すべて同じ値）
- Noise ハンドシェイクと `JarvisProtocol` の delta イベント 23 種（追加も削除も無し）
- チャット系経路、認証系 REST 経路、`hatch-api.meta.ai`

追加された jarvis エンドポイント 20 件、削除 2 件、追加ヘッダ 7 件、
新規パッケージ 6 件。いずれもこのクライアントが使う経路ではない。

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

## DSH との対話（muse-relay）

DSH と Muse の往復には、両方向とも同じ形式のヘッダを付ける。Muse → DSH 方向は
dsh-session-api の Muse 窓が付与し、DSH → Muse 方向は **このサーバが付与する**。
モデルに書かせない。書かせると文面が揺れて機械的に読めなくなる。

```
[muse-relay]
  museSessionId: <宛先の Muse チャットの ID>
  dshSessionId: <送信元 dsh セッションの ID>
  requestId: <相関 ID>
```

同じ3値はワイヤ上のヘッダにも載る（`museSessionId` / `dshSessionId` / `requestId`）。
`requestId` はトランスポートの `x-request-id` にも流用する。

```
muse relay "本文" \
  --muse-session 002486c1-6c8e-4228-a5f9-35005acf9a17 \
  --request-id muse-mcp-relay-reverse-20261115-b206576f-1
```

MCP は `muse_relay(prompt, museSessionId, dshSessionId, requestId, reply_timeout_sec)`。
引数名はワイヤと同じ camelCase で、`prompt` だけは既存名を変えていない。

### サーバが決めること

| 項目 | 扱い |
|---|---|
| `museSessionId` | 必須。小文字 UUID でなければ配送しない |
| `dshSessionId` | 省略時は `DSH_SESSION_ID` から埋める。引数と環境が食い違えば配送しない。両方無ければ配送しない |
| `requestId` | 渡された値はそのまま使う（Muse 発の依頼への返信）。無ければ `dsh-<uuid>` を生成 |

3項目がそろわない限り配送しない。ヘッダ無しで送る後方互換は無い。

### 本文側の偽装と重複

本文に `[muse-relay]` ブロックが入っていた場合、サーバの解決値と照合する。

- 値が違う → 配送しない（本文とヘッダが矛盾するメッセージは通さない）
- 項目が欠けている → 配送しない
- 値が同一 → 本文から取り除き、サーバのブロック1つだけにする

旧来の「DSH セッションから送っています」式の定型文はサーバでは付けない。

### 監査

配送のたびに、状態ファイルと同じディレクトリの `relay-audit.jsonl` へ1行追記する。
記録するのは `museSessionId` / `dshSessionId` / `requestId` / 配送結果だけ。
本文は記録しない（監査の範囲を本文全文へ広げない）。

```json
{"at_ms":1794715148670,"delivered":true,"detail":"7050114c-...","dshSessionId":"b206576f-...","museSessionId":"002486c1-...","requestId":"muse-mcp-relay-reverse-20261115-b206576f-1","tool":"muse relay"}
```

## 必須化の範囲

`[muse-relay]` の付与は `muse_relay`（CLI は `muse relay`）に限る。`muse_ask` と
`muse chat` は通常の会話経路であり、ヘッダは付かない。拡張しない判断は確定済みで、
これが仕様であって未対応ではない。

- 対象: `muse_relay` / `muse relay`
- 対象外: `muse_ask` / `muse chat` / `muse ask`

対象外の経路で Muse に届いたメッセージは、どの DSH セッションから出たかを本文から
機械的に読めない。相互に突き合わせたい場合は `muse_relay` を使う。

## プラットフォーム

Windows と Linux の両方でビルド・実行を実測している。macOS はソースが同じ経路を通るが未検証。

| 項目 | Windows | Linux |
|---|---|---|
| ビルド | `cargo build --release` | 同左（Debian 13 / rustc 1.99.1 で実測） |
| テスト | 126 件 green（Windows） | 124 件 green（Debian 13 / rustc 1.99.1、Unix専用1件を含む） |
| 状態ファイル | `%APPDATA%\muse-mcp\session.json` | `$XDG_CONFIG_HOME/muse-mcp/session.json`、無ければ `$HOME/.config/...` |
| 状態の権限 | 既定の ACL | 0600（ファイル）と 0700（ディレクトリ） |
| 保存方式 | 一時ファイル→rename | 同左 |

状態ファイルは所有者だけが読める権限で、書き込みは一時ファイル経由の rename で行う。
中断してもセッションが半端な JSON で壊れない。

`MUSE_MCP_STATE` を指定した場合はそのパスをそのまま使う。`HOME` も `XDG_CONFIG_HOME` も無い
環境では、ホーム直下を汚さないよう作業ディレクトリの `.muse-mcp-session.json` に置く。

## 既知の挙動

`muse chat` の応答取得は履歴ポーリングで、送信した `message_id` を起点に探す。
送信メッセージが履歴の窓より古い場合は「まだ応答が無い」として待ち続け、
別ターンの応答を返さない（実測で確認済みの回帰項目）。

## 制約

- Muse の非公開プロトコルに依存する。上流の変更で壊れうる
- システムファイルはエージェントのシェル実行に依存する。1回につきエージェントのターンを1回消費し、
  直接HTTPより遅い
- `/fs/*` はワークスペース外に出られない。サーバー仕様であり迂回しない
- SSH operator は DNS 未解決で到達できない
- ACL で閉じている経路（`/api/tasks/shell`、`/api/navigation/tab-order`、`/api/connectors` POST 等）は迂回しない
- 承認（`/api/approvals*`）と権限設定（`/api/permissions/*`）はサーバーに存在しない
- confidential VM の attestation は復号のみで、完全な検証チェーンは実装していない
