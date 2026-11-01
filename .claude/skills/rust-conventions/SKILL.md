---
name: rust-conventions
description: This skill should be used whenever writing, editing, or reviewing Rust code (`*.rs`, `Cargo.toml`) in this repository (cafce) — e.g. "Rustのコードを書いて", "この関数を実装して", "このRustコードをレビューして". Encodes this project's opinionated Rust house rules ("RealWorld業務Rust"): import/naming style, error handling, concurrency, logging, testing, and dependency practices. Consult before writing new Rust code, refactoring, or reviewing a Rust diff.
---

# Rust コーディング規約 (cafce / RealWorld業務Rust)

このプロジェクトの Rust コードは、個人のライブラリ開発ではなく複数人で保守する業務アプリケーションコードとして書く。「誰が読んでも理解でき、どこにでもコピペで移植できる愚直で平易なコード」を最優先し、実行効率や"賢い"抽象化より可読性・可保守性を優先する。

詳細な理由・引用元・具体例は `references/read_world_rust_part.md` を参照。ここでは新規コード作成・レビュー時に守るべきルールだけを要約する。

## CI / 開発環境

- `cargo fmt --check` と `cargo clippy --tests --examples -- -Dclippy::all` を通す。warningを1つも残さない。
- 依存crateのsemverを信用しない。パッチバージョンまで固定する。`0.x`系は破壊的変更前提、`1.x`系でも油断しない（`aws-sdk-rust`のような例がある）。
- コンパイルエラー・clippy警告は一番上のものから順に直す（`cargo clippy --tests --examples 2>&1 | head -n 40` を優先的に見る。下の方のエラーは上のエラーの副産物であることが多い）。
- releaseビルドでも `[profile.release] debug = 1` を維持し、`RUST_BACKTRACE=1` 前提で運用する。バックトレースが効く前提なら `.expect("...")` を頑張って書かず `.unwrap()` でよい。

## import / 命名

- `use hoge::A as HogeA;` のようなaliasやtrait aliasは使わない。フルパス（`hoge::A`）で書く。
- `use anyhow::Result;` のように `std` の型名を覆う `use` は特に避ける。
- ファイル先頭・モジュール先頭での `use` は基本使わない。`std::rc::Rc<std::cell::RefCell<T>>` のようにフルパスで書く。
  - 例外: `use std::rc::Rc;` / `use std::sync::Arc;` のような衝突しにくいものは許容する。
  - 例外: 関数内スコープの先頭での `use` はスコープが狭くコピペ可植性が高いため許容する。
  - 例外: `#[cfg(test)] mod tests { ... }` のモジュール先頭での `use super::*;` のみ許容する（Rustの標準的な書き方であるため）。ただし`use crate::hoge::Huga;`のような個別importはこの例外に含めない。テスト内で他モジュールの型が欲しい場合は、フルパスで書くか、複数回参照して繰り返しがつらいなら上記の「関数内スコープの先頭でのuse」を使う。
- `use hoge::prelude::*;` のようなワイルドカードimportは使わない（`use super::*;`の例外を除く）。
- crate名は `hoge_huga`（アンダースコア区切り）で統一し、`hoge-huga` とは書かない。

## 設計・抽象化

- 複雑なライフタイム変数を持つ参照は避け、`Arc<Mutex<T>>` + `Clone + Send + Sync + 'static` に倒す。業務コードでは実行効率よりコピペ可能な単純さを優先し、最適化は問題が起きてから行う。
- 独自traitでDSLを作らない。マクロも避ける。O(n)の置換コピペで修正できる愚直なコードを、誰にも読めない「完璧な」抽象化より優先する。
- Builder Patternは避ける。非公開の内製crateでは、setterを生やすより Parameter struct を受け取る `new` メソッドだけで十分とする。

## エラーハンドリング

- `hoge.huga()?` をそのまま使わず、`anyhow::Context` の `.context(...)` を付けて失敗箇所が追えるようにする。
- 回復可能なエラーと回復不能なエラー（panic相当）を分ける。回復不能なものを `anyhow::Error` に括り出しつつ `?` を使えるよう、`Result<Result<T, CustomError>, anyhow::Error>` のようにネストさせてよい。
- `let _ = hoge();` で結果を握りつぶさない。無視する変数には `_hoge` のように名前を付ける（`_` 単体は変数束縛ではなくその場でdropされるパターンマッチであり、意味が異なる）。

## ロギング・テスト

- `println!` ではなく `log::debug!` 等を使う。`main` には `env_logger` を入れ、テストには `env_logger::builder().is_test(true).try_init().ok()` を入れる。
- I/Oを伴うテストはflakyになりがちなので、可能な限りI/Oをモックする。一方でE2Eテストも用意する（実行時リンクエラーなど、E2Eでしか検出できない問題がある）。

## 非同期

- `async fn` のシグネチャに頼りすぎない。関数が返すfutureが持つ参照のライフタイムを明示したい場合は、`#[allow(clippy::manual_async_fn)]` を付けて `fn foo<'a>(...) -> impl Future<Output = T> + Send + 'a` の形で書く。

## Additional Resources

- `references/read_world_rust_part.md` — 各ルールの背景・理由・引用元リンクを含む元ドキュメント全文
