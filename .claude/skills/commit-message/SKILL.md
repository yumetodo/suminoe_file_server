---
name: commit-message
description: This skill should be used whenever Claude is about to write a git commit message in the cafce repository (e.g. the user says "commitして", "コミットして", "commit this"). Provides this project's commit message subject/body content conventions, derived from this repo's own commit history. Complements — does not replace — the harness's general git commit workflow (staging, confirmation, hooks, --no-gpg-sign等).
---

# コミットメッセージ規約 (cafce)

このスキルは cafce リポジトリの全コミット履歴（`git log --all`、45件）を観察して抽出した、コミットメッセージの**内容・文体**の規約を提供する。ステージングや確認手順などgit操作自体の手順は、別途Claude Codeの標準のコミット手順に従う。ここではメッセージ本文の書き方のみを扱う。

## Subject行（1行目）

`<type>: <日本語の説明> (#<issue番号>)` の形式にする。

- **type**: `feat` / `fix` / `docs` / `test` / `chore` のいずれか（英語小文字）。必要なら `feat(test):` のようにスコープを付けてもよい。
- **説明**: 日本語、体言止め（「〜を追加」「〜を修正」のように「した」を省いた名詞形）で終える。文末に句点は付けない。
- **issue番号**: 関連issueがあれば行末に半角スペース1つを空けて `(#N)` を付ける（例: `docs: 設計docのWork Logを実際の実装結果に合わせて更新 (#2)`）。関連issueがなければ省略してよい。
- type prefixなし・英語のみのsubjectは、このリポジトリ最初期（`init project` など）や外部コントリビューターのPRにのみ見られる例外であり、通常のコミットでは踏襲しない。

## Body（本文）

- 変更がsubject行だけで説明しきれる軽微なものであれば本文なしでよい（実際、過去コミットの約半数は本文なし）。
- 本文を書く場合は箇条書きではなく**日本語の散文**で、次の順に書く。
  1. 背景・問題（何が困っていたか、なぜ変更が必要か）
  2. 何をどう変更したか
  3. 影響範囲・検証状況（テスト結果、確認した動作など）
- 変更ファイルを列挙する必要があるときだけ、`- ファイル名: 内容` の1階層ダッシュ箇条書きを使ってよい。
- 設計docの節番号や関数名など、読み手が該当箇所を追える具体的な識別子を本文中で引用してよい。

## フッター

- Claude Codeでコミットを作成する場合の `Co-Authored-By:` / `Claude-Session:` 行は、既存のコミット手順に従い自動的に付与される。このスキルはそれ以外の追加フッターを指示しない。

## 避けるべきこと（過去の非典型例からの教訓）

- `(#N)` の前のスペースを省略しない（`fix: bump rust to 1.94.1(#2)` のような表記ゆれが過去にあるが踏襲しない）。
- typoをそのまま残さない（過去に `desgin`, `gtilab` のようなtypoがsubjectに残った例があるため、コミット前に見直す）。
- 同一内容のコミットを連続で作らない。
