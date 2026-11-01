<!-- オリジナル版リンク https://gist.github.com/legokichi/89fac9c5be0eb0e50972da85cc8ee4ed -->

# RealWorld 業務 Rust

- 実際に Rust 1.0 の頃から業務で Rust を使ってコードを保守してきてハマった落とし穴についての ~~知見~~ 恨み言です
- Rustが素晴らしい言語であるというあたりまえのことにはこの文書では触れません
- 気が向いたら追加します

## 開発環境編

### CI で `cargo fmt` をチェックしろ

- 常識
- オレオレフォーマットを主張するやつは相手にするな
- ci に ~~`cargo fmt && git diff --exit-code` とかすればよい~~
  - いまは `cargo fmt --check` で同等のことができます

### CI で `cargo clippy --tests --examples -- -Dclippy::all` しろ

- とりあえず全部つけとけ
- パラノイアになれ
- 長いものにまかれろ
- ひとつの warning も許すな

### crate の semver は信じるな

- rust において信じられるのは semver ではない、作者への信用だけだ
- 0.x はすべて破壊的変更を含んでいると思え
- 1.x になっても非互換な変更を入れてくるやつはいる、aws-sdk-rust とか

### バージョンはパッチバージョンまで固定しろ

- パッチバージョンを上げただけでバグる crate は存在する
- aws-sdk-rust とか

### rust-analyzer は頼りにならない

- 小規模コードならともかく、コードが大きくなるにつれて動かなくなる
- `features` を認識しなかったりバージョン違いなどで、いずれ動かなくなる
- 複数回のコード定義ジャンプしないと理解できないようなコードを書くな
- 「n回のコピペ（切り貼り）で移植可能なコード」のnが小さいほど可読性、可植性が高い
- 誰でも読めてどこにでもコピペできる愚直で平易なコードが長く生き残るコードだ
- https://play.rust-lang.org/ にコピペして実行できるサイズのコードがちょうどよいサイズだ

### コンパイルエラーは一番上のエラーから順番に直せ

- `cargo clippy --tests --examples 2>&1 | head -n 40` だけを信じろ
- 下の方のエラーは上のエラーが引き起こしているので読むだけ無駄である

### 本番環境でも常に `RUST_BACKTRACE=1` で実行しろ、

- release にも debug 情報は残せ
- ```toml
  [profile.release]
  debug = 1
  ```
  しろ
- [error-chain](https://docs.rs/error-chain/latest/error_chain/) も [failure](https://docs.rs/failure/latest/failure/) も [thiserror](https://docs.rs/failure/latest/thiserror/) も信用できない
- 結局信じられるのは stack trace の関数名と行番号だけだ
- backtrace が有効なら`.expect("ここで落ちた")` を頑張る必要はない
  - 安心して `.unwrap()` してくれ

## コーディング編

- RealWorld 業務 Rust は個人のライブラリ開発ではない
- この文書は社内のみの業務アプリケーションコードを複数人で書くコツである

### alias は使うな

- `use hoge::A as HogeA;` とかするな
- 愚直に `hoge::A` とフルパスでタイプしろ
- お前は読めても他の人間は読めない
- `type Result<T> = Result<T, MyError>` みたいな std の型名を上書きするのは論外
- お前のことやぞ `use anyhow::Result;`
- `use std::error::Error;` と `use std::io::Error;` を見分けられる人間だけが石を投げなさい
  - `use std::time::Duration;` と `use chrono::Duration;` もあるぞ
  - `use thiserror::Error:` と `use anyhow::Error:`もあるぞ
  - 等々
- お前は読めても他の人間は読めない
- trait alias も同様
- お前は読めても他の人間は読めない
- 部分コピペで動かなくなるコードは作るな

### ファイル先頭で `use` は使うな

- `use std::sync::mpsc::channel;` とかするな
  - `channel` とかの一般名刺が突然出てきてもわからなくなる
- `use hoge::Error;` とかするな
  - `std::error::Error` と区別がつかなくなるから 
  - 部分コピペで動かなくなるコードは作るな
- お前は読めても他の人間は読めない
- `std::rc::Rc<std::cell::RefCell<T>>` とか `Box<dyn std::future::Future<Output=T>+ Send + Sync + 'static>` とかをノーミスでタイプできるようにしろ
  - タイピング練習を欠かすな
- でも `use std::rc::Rc;` とか `use std::sync::Arc;` とかならゆるしちゃうかも
  - メジャーなライブラリでの名前の衝突がないので
- でも `use tokio::sync::Mutex;` とかが突然生えてきたりするので自衛のために愚直に `std::sync::Mutex<T>` と書いてしまおう
- 関数内の先頭なら許す
  - ファイル先頭と違ってスコープが狭いので
  - コピペ可植性が高いので
- でもモジュールの先頭とかには書かないでくれ
- コードを書くときは楽ができるかもしれないが、コードを保守する側としては大変困る
- github で PR を作ってもファイル先頭 `use` はコンフリクトの主要な発生源になり大変面倒
- `use std::{thread::sleep, *}` みたいなワイルドカード import は言語道断である
- [Smithay/smithay のこのコードを見て発狂しないやつだけが石を投げなさい](https://github.com/Smithay/smithay/blob/8e779d02a3e4f4a467b71965bf18b174dca80d4a/smallvil/src/state.rs#L3)

### `use hoge::prelude::*`  は使うな

- ファイル先頭に限らず `std` 以外の `prelude` は使うべきでない
- どのシンボルが import されているのか、そのライブラリに詳しいお前以外は予想できない
- お前は読めても他の人間は読めない

### 複雑なライフタイム変数を持つ参照は使うな

- 脳死で `Arc<Mutex<T>>` して `Clone + Send + Sync + 'static` しろ
- 業務で生体参照ライフタイムソルバするのは不毛
- メモリ効率とか速度とか気にするな
- 顧客へのデプロイ速度がすべてだ
- 実行効率の最適化は問題が起きてからやれ
- 非同期rustを書いていると `String` の `clone` が頻発する（&strがライフタイム的にできないので）が、気にせず `clone` しろ
  - `Arc<String>` を使う最適化は後から考えろ

### オレオレ trait は使うな

- trait でオレオレ DSL を作ろうとするな
- お前は読めても他の人間は読めない
- rust-analyzer で定義に飛んで trait だったときの絶望感を味わえ
- 誰にも読めない完璧な抽象化コードよりも、誰でも読めてどこにでもコピペできる愚直で平易なコードが長く生き残るコードだ
- [「悪い方が良い」原則](https://note.com/ruiu/n/n9948f0cc3ed3) を信じろ

### マクロは使うな

- お前にしか読めないコードよりも誰でも読めるコピペコードのほうがマシだ
- 修正箇所が O(n) の置換コピペで済むならコピペコードのほうがマシだ
- 誰にも読めない完璧な抽象化コードよりも、誰でも読めてどこにでもコピペできる愚直で平易なコードが長く生き残るコードだ
- [「悪い方が良い」原則](https://note.com/ruiu/n/n9948f0cc3ed3) を信じろ

### io をモックできるテストを書け

- io を伴うテストにはすべからく再現性がない(flaky である)

### e2e テストを書け

- aws-sdk-rust すら実行時の挙動に破壊的変更が入る
- aws lambda を aws_lambda_runtime で実行する場合 amazon linux で実行することになるが、 glibc のバージョン違いとかでローカルテストが通っても lambda の中では実行時リンクエラーで動作しなかったりする
- e2e test だけが唯一信用できる


### `huga()?` みたいな [(the question mark operator)](https://github.com/rust-lang/rfcs/blob/master/text/0243-trait-based-exception-handling.md) をそのまま使うな

- エラーを見てもどこで何がおきたかわからん
- `use anyhow::Context;` して `hoge.huga(param).context(format!("huga {param:?} で落ちた"))?` を書きまくれ
- backtrace も有効化しろ

### Builder Pattern はクソ

- Rust の [Builder Pattern](https://www.ameyalokare.com/rust/2017/11/02/rust-builder-pattern.html) は crate で API を公開するときのメジャーバージョンの互換性のために使われている
- 非公開内製 crate なら Builder Pattern で setter を生やすよりも Paramater struct を引数にとる new method だけで十分
- お前のことやぞ　[aws-sdk-rust](https://docs.rs/aws-sdk-dynamodb/latest/aws_sdk_dynamodb/)
- [rusoto](https://docs.rs/rusoto_dynamodb/latest/rusoto_dynamodb/) は良かった、本当に…
- [init pattern](https://xaeroxe.github.io/init-struct-pattern/) が好き

### println するな log::debug しろ

- [log](https://docs.rs/log/latest/log/) を使っておけばテストやデバッグでも潰しが効く
- [tracing](https://docs.rs/tracing/latest/tracing/) にも対応できるぞ
- とりあえず main 関数には脳死で [env_logger](https://docs.rs/env_logger/latest/env_logger/) 入れとけ
- テストにも脳死で `env_logger::builder().is_test(true).try_init().ok()` って書いとけ
- これが業務 rust の "おまじない" だ


### エラーの型は `Result<Result<T, CustomError>, anyhow::Error>` でFA

- Rustのエラー処理はResultのネストが正解
- エラーには分類（回復）可能なものとそうでないもの(panic相当)がある
- 回復不能なものを別にanyhowとしてくくりだすことで `?` を使いつつ柔軟なエラー処理が書ける
- ```rust
  #[tokio::main]
  async fn main() -> Result<(), anyhow::Error>{
    let o = match foo().await? {
      Ok(o) => o,
      Err(CustomError::A) => {
        todo!()
      }
      _ => {
        todo!()
      }
    }
  }
  ```
- 単にpanicさせるのではなくエラーレポートを書きたいなどのときに、パニックハンドラのようなlow-levelの処理に頼らなくても良くなるので便利

### `let _ = hoge()` による `_` 束縛は使うな `_hoge` みたいに名前をつけろ

- `_` で束縛した変数は実は束縛されず、その場で drop される
- これは実は変数束縛ではなくパターンマッチング
- 変数スコープを抜けるときに drop されれる他の変数とは処理が異なる
- ややこしいから unused variable warning を避ける目的なら `_hoge` のように名前をつけろ
- 公式ドキュメントにも RFCS にも "明示的には" 載ってない挙動です
- [Ignoring an Entire Value with _
](https://doc.rust-lang.org/stable/book/ch18-03-pattern-syntax.html#ignoring-values-in-a-pattern)
- [wildcard-pattern](https://doc.rust-lang.org/reference/patterns.html#wildcard-pattern)
- [[Rust] _(underscore) Does Not Bind](https://medium.com/codechain/rust-underscore-does-not-bind-fec6a18115a8)
- [Rustにおけるirrefutable patternを使ったイディオム](https://blog.idein.jp/post/644161837029605376/rust%E3%81%AB%E3%81%8A%E3%81%91%E3%82%8Birrefutable-pattern%E3%82%92%E4%BD%BF%E3%81%A3%E3%81%9F%E3%82%A4%E3%83%87%E3%82%A3%E3%82%AA%E3%83%A0)
- [destructors](https://doc.rust-lang.org/reference/destructors.html#temporary-scopes)


### `async fn` はあてにならない

- `async fn` の返り値の future が持つ参照のライフタイムは記述できない
- clippy が `warning: this function can be simplified using the async fn syntax` とか言ってくるが `#[allow(clippy::manual_async_fn)]` で黙らせろ

[async fn が使えないので sqlx のクエリ関数の例](https://github.com/launchbadge/sqlx/pull/1687/files#diff-9a071fcf9db3c54bc8e2179258161fccd6d7fc8cf3d2c6ae390213c2e87b9bf8R38)

```rust
#[allow(clippy::manual_async_fn)]
fn run_query<'a, 'c, A>(conn: A) -> impl Future<Output = Result<(), BoxDynError>> + Send + 'a
where
    A: Acquire<'c, Database = Postgres> + Send + 'a,
{
```

### crate 名は常に `hoge_huga` を使え `hoge-huga` は使うな

- ややこしい
- `serde-json` か `serde_json` か間違えたことがないものだけが石を投げなさい

### 長いものに巻かれろ

- 一番使われている crate がいちばんいい crate だ
- 謎の crate を自作するな公開するな
