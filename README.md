# 魔法少女ノ魔女裁判 音声データセットビルダー

対応作品は [魔法少女ノ魔女裁判（Steam）](https://store.steampowered.com/app/3101040/) です。この Rust CLI は、利用権限のあるローカルのゲームファイルから日本語の台詞と音声を対応付け、監査記録付きの Parquet データセットを作成・検証します。ゲーム本体、音声、台詞、生成済みデータセットは、このリポジトリと配布バイナリに含まれません。

入力は `config/manosaba-inputs-v1.json` で承認された **macOS 版 `manosaba.app` の特定ビルド**に限定されます。承認済みのファイル構成やハッシュが異なる場合、`build` は処理を止めます。ゲームのファイルは読み取り専用で扱い、出力先はアプリの外にある未使用のパスを指定します。

## バイナリの入手

公開されたバイナリは [GitHub Releases](https://github.com/tkgstrator/mgwt-voice-dataset-builder/releases) から入手できます。リリースには `SHA256SUMS` も添付されます。

| 実行環境 | アーカイブ |
| --- | --- |
| Linux amd64 | `mgwt-voice-dataset-builder-linux-amd64.tar.gz` |
| Linux arm64 | `mgwt-voice-dataset-builder-linux-arm64.tar.gz` |
| macOS Intel | `mgwt-voice-dataset-builder-macos-amd64.tar.gz` |
| macOS Apple silicon | `mgwt-voice-dataset-builder-macos-arm64.tar.gz` |
| Windows x64 | `mgwt-voice-dataset-builder-windows-amd64.zip` |

Linux では、ダウンロードしたアーカイブの名前を `archive` に設定してチェックサムを検証します。

```sh
archive=mgwt-voice-dataset-builder-linux-amd64.tar.gz
grep -F "  $archive" SHA256SUMS | sha256sum -c -
```

macOS では `shasum -a 256 <アーカイブ名>` の結果を `SHA256SUMS` の対応する行と照合してください。確認後にアーカイブを展開し、作成された `mgwt-voice-dataset-builder-<platform>` ディレクトリに移動して `./mgwt-voice-dataset-builder --help` で起動を確認できます。バイナリには承認済み入力の設定が組み込まれているため、ビルド用のソースチェックアウトは不要です。

Windows では PowerShell の `Get-FileHash .\mgwt-voice-dataset-builder-windows-amd64.zip -Algorithm SHA256` の結果を `SHA256SUMS` の対応行と照合し、ZIP を展開します。展開後は `mgwt-voice-dataset-builder-windows-amd64` ディレクトリ内の `mgwt-voice-dataset-builder.exe` を実行します。入力として承認されているのは、Windows にコピーした上記の macOS 版 `manosaba.app` のファイル構成です。Windows 版 Steam インストールの別構成は承認対象に含まれません。Windows の出力先・作業領域にはローカルの ACL 対応ボリュームを指定してください。ネットワーク共有への出力は対象外です。

## 使い方

ソースから実行する場合は Rust 1.88.0 が必要です（`rust-toolchain.toml` で固定）。リポジトリのルートで、以下のパスを自分の環境に合わせて指定してください。配布バイナリを使う場合は、各行の `cargo run ... --` を実行ファイルのパスに置き換えます。`build` の出力先は、まだ存在しないディレクトリ名にします。

```sh
APP=/path/to/manosaba.app
OUTPUT=/path/to/manosaba-voice-dataset

cargo run --locked -- inspect --app "$APP" --output /path/to/candidate-inventory
cargo run --release --locked -- build --app "$APP" --output "$OUTPUT"
cargo run --release --locked -- verify --input "$OUTPUT"
```

`inspect` は承認済み入力と実際のファイルを比較し、候補インベントリと診断情報を新しい出力先へ書き込みます。`build` は承認済み設定からデータセットを作り、最終検証に成功した場合だけ出力先へ昇格します。`verify` は完成したローカルデータセットを読み直します。`build` には `--shard-audio-mib`（既定値 500）と `--bundle-workers` を指定できます。各コマンドの詳細は `--help` で確認できます。

出力は `data/ja/<character_id>/train-*.parquet`、`metadata/` 内の監査・拒否・マニフェスト・レポート、およびデータセット用 `README.md` です。Windows の `build` が途中で失敗すると、部分データを含む作業ディレクトリを安全確認のため保持し、そのパスを標準エラー出力に表示します。再実行前に表示されたパスと所有者を確認してください。終了コードは成功 `0`、処理・検証エラー `1`、入力・パス・設定エラー `2` です。

## 開発環境

VS Code の **Reopen in Container** から `.devcontainer/` を使えます。Ubuntu 24.04 と Rust 1.88.0 を使用し、Cargo の依存関係とビルド成果物を専用ボリュームに保存します。コンテナにはゲームデータやホストの認証情報を自動でマウントしません。`inspect` や `build` をコンテナ内で実行する場合は、自分が利用権限を持つ `manosaba.app` を別途、読み取り専用でマウントしてください。通常のテストと `verify` はゲームデータなしで実行できます。

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo doc --workspace --no-deps --locked
```

このツールのコードは MIT ライセンスです。ゲームとその音声・台詞の権利は、それぞれの権利者に帰属します。生成物の利用や共有には別途、必要な権利を確認してください。

## English summary

This Rust CLI inspects an approved local **macOS Steam build** of [Magical Girl Witch Trials](https://store.steampowered.com/app/3101040/), builds an auditable Japanese voice dataset, and verifies the completed output. Game assets and generated datasets are not distributed with this repository or its binaries. The checked-in input fingerprint does not approve other game builds or platforms.
