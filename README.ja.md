# jev-find (`jfd`)

[English](README.md) | 日本語

[Jev](https://docs.typesafe.ai/api) を探索の中核に置いた、自然言語で探せる find ライクなファイルファインダ。

```
jfd "OAuthのトークン更新を実装しているコード" src/
jfd "壊れていそうな設定ファイル" -e yaml -e toml -p
jfd "テストコード" -c unit=単体テスト -c e2e=E2Eテスト
jfd "セキュリティ上危なそうな処理" --rank 危険度 --levels 低,中,高
jfd "予算について話した会議メモ" ~/Documents -n 10 -p   # 確率の高い上位10件
```

## インストール

macOS (Apple Silicon / Intel)・Linux (x86_64 / arm64) はビルド済みバイナリを入れられます:

```bash
curl -fsSL https://raw.githubusercontent.com/masa-whitestone/jev-find/main/install.sh | sh
```

`~/.local/bin/jfd` に入ります(`JFD_INSTALL_DIR` で変更可)。Rust があればソースからも入れられます:

```bash
cargo install --git https://github.com/masa-whitestone/jev-find
```

## セットアップ

TypeSafe の API キーが必要です([typesafe.ai](https://typesafe.ai) で発行)。

```bash
jfd setup
```

表示言語 (English / 日本語)、APIキー(非表示で入力)、モデルを聞かれ、疎通確認のあと `~/.config/jfd/config.json`(権限 0600)に保存します。
キー未設定のまま `jfd` を端末で実行した場合も、自動でこのセットアップが始まります。

| コマンド | 内容 |
|---|---|
| `jfd config` | 現在の設定を表示(キーは伏せ字) |
| `jfd config set-key` | APIキーを変更(非表示入力。`-` で標準入力から) |
| `jfd config set-lang en\|ja` | 表示言語を変更 |
| `jfd config set-model <MODEL>` | 既定モデルを変更 |
| `jfd config set-base-url <URL>` | APIのベースURLを変更 |
| `jfd config set-source <DIR>` | `jfd update` にソースから再ビルドさせる |
| `jfd config unset key\|lang\|model\|base-url\|source\|all` | 既定値に戻す |
| `jfd config test` | APIに接続できるか確認 |
| `jfd config path` | 設定ファイルのパス |

優先順位は **コマンドライン > 環境変数 (`TYPESAFE_API_KEY` / `JEV_MODEL` / `TYPESAFE_BASE_URL`) > 設定ファイル > 既定値**。
`setup` や `config` という語そのものを検索したいときは `jfd -- config` と書きます。

## アップデート

```bash
jfd update          # 最新版に更新
jfd update --check  # 新しい版があるか確認するだけ
```

- ビルド済みバイナリで入れた場合: GitHub の最新リリースをダウンロードし、SHA-256 を確かめてから入れ替えます。
- ソース(git のチェックアウト)から入れた場合: `git pull` してから `cargo install` で入れ直します(`--no-pull` で pull を省略)。

## 送信されるデータ

判定のため、ファイルのパス・サイズ・更新日と、必要なときはファイルの先頭 4KB(`-H` で変更)を TypeSafe の API に送ります。
機密ファイルがある場所では、探索範囲を絞るか `-E` で除外してください。
API 手前のファイアウォールに内容を遮断されたファイルは、名前とメタデータだけで判定されます。

## 仕組み

1. 並列ウォーカーでファイルを列挙(`--prune` でディレクトリ単位に Jev へ枝刈りを問う)
2. 段階1: パス・サイズ・更新日だけで判定(内容を読まない・安い)
3. 段階2: 決めきれないものだけ先頭数KBを読んで判定。分類 (`-c`) と採点 (`--rank`) も同じリクエストで問う。
   まず先頭1KB (`--peek-bytes`) だけを見せ、それで決まらないとき (`--peek-reject` / `--peek-accept`) だけ `-H` バイトまで見せて問い直す。こうして決まった結果は `--json` で `"stage": "peek"` になる。
   トークンを節約するため、判定の基準は1リクエストに1回だけ書き、ディレクトリのパスはバッチ内で1回だけ送る

問い合わせはディレクトリをまたいで束ね、1リクエストで段階1は最大64件 (`--batch`)、段階2は最大16件 (`--deep-batch`) を判定します。
トークン上限を超えたバッチは自動で半分に割って送り直します。
バッチ化は適応的です。ファイルの流れが数ms途切れた時点で送るので、固定の待ち時間はありません。`-j` の同時実行枠が埋まっている間は項目がたまり、枠が空いたときに大きなバッチでまとめて送ります。
`--prune` では、親ディレクトリの判定を待たずに子ディレクトリの一覧と枝刈りの問い合わせを始めます(1段だけ先読み)。深い木でも、1段ごとにAPIの往復を待つことはありません。

回答はファイル単位で `~/.cache/jfd/` の追記ログ1本にキャッシュされ(起動時に裏で読み込みます)、ファイルが変わると自動で無効になります。
そのため `--budget N`(リクエスト数の上限)で途中で打ち切られても、再実行すれば続きから進みます。
`--stats` で消費トークンと推定コストを表示します。

`--rank` のスコアは 0(最低の段階)〜1(最高の段階)で、最も近い段階名も表示されます。

### 既定で飛ばす場所

- `~/Library`、`node_modules`、`__pycache__`、`site-packages`、`DerivedData`、`Pods`
- アプリなどのバンドル (`.app` `.framework` `.photoslibrary` など)
- `CACHEDIR.TAG` のあるディレクトリ(cargo の `target/` などのビルド出力)
- 隠しファイル・隠しディレクトリ(`--hidden` で対象に)

`--no-ignore` で全部降ります。権限のないディレクトリ(macOS の保護フォルダ)は黙って飛ばし、件数だけ表示します(`-v` で一覧)。

## ライセンス

MIT
