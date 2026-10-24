# MGWT 音声データセットビルダー

ローカルで利用権限のあるゲームファイルから、音声データセットを作成するための Rust CLI です。現時点では CLI と安全なパス検証の基盤のみ実装されています。`inspect`、`build`、`verify` の処理本体は未実装で、検証後に処理エラーを返します。ゲームのファイルや生成済みデータセットはリポジトリに含まれません。

## 必要な環境

- Rust 1.88.0（`rust-toolchain.toml` で固定）
- macOS または Linux
- 利用権限のあるローカル入力と、入力の外にある空の出力先

## コマンド

```text
mgwt-voice-dataset-builder inspect --app <authorized-app> --output <new-candidate-dir>
mgwt-voice-dataset-builder build --app <authorized-app> --output <new-dataset-dir>
mgwt-voice-dataset-builder verify --input <dataset-dir>
```

将来の `inspect` は承認候補を作成し、`build` はレビュー済み設定を必要とします。出力先は存在してはならず、入力ディレクトリと重複・包含関係にできません。シンボリックリンクを含むパスは拒否します。利用に必要なディスク容量と設定承認手順は、処理本体の実装時に文書化します。

終了コードは成功 `0`、処理・検証中の失敗 `1`、入力・パス・設定の検証失敗 `2` です。失敗時は出力先を再確認し、所有権が確認できないファイルを削除しないでください。

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo doc --no-deps --locked
```

MIT ライセンスです。現在の主な依存関係は [Clap](https://crates.io/crates/clap) と、記述子相対のファイル操作を提供する [rustix](https://crates.io/crates/rustix) 1.1.5 です。将来の Unity 読み取り依存関係 `unity-rs-core` は、設計で指定した Git リビジョン `b3e509ed09fe6ff3a897d04ddb75eb5214482265` に固定して導入する予定です。

## English summary

This is a Rust CLI foundation for turning locally authorized input into a voice dataset. Only argument and path validation are implemented so far. No game assets or generated datasets are distributed here.
