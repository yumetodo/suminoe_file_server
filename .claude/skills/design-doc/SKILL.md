---
name: design-doc
description: This skill should be used when the user asks to "write a design doc", "create a design doc", "design docを書いて", "設計ドキュメントを作成して", "設計docを書きたい", or references `doc/design/` or `doc/llm_design_doc_template.md`. Use for authoring a new Design Doc for this project (cafce) under `doc/design/`, following the project's template and existing conventions. Not for ADRs — this project does not use ADRs.
---

# Design Doc 作成 (cafce)

このスキルは、cafce プロジェクトにおける Design Doc（設計文書）を `doc/design/` 配下に作成するための手順を提供する。テンプレートは `assets/llm_design_doc_template.md` にある。

## Design Doc の位置づけ

- Design Doc は**実装前に設計の意図・背景・トレードオフを記録し、合意形成を促す文書**である。「未来の自分や仲間が、その設計判断を再現できるか」への回答集として書く。
- このプロジェクトは **ADR を採用しない**。「なぜこの案を選んだか」という意思決定の文脈は、ADR のような決定台帳としてではなく、Design Doc 内の「Alternative Solution」節に**設計の物語**として書く。
- **有効期間はそのDesign Docが作られたブランチが生きている間のみ。** ブランチがマージ・削除された後は、その Design Doc を最新化・更新の対象にしない。実装後に設計が変わった場合も、Design Doc を書き換えずに「作成時点の設計意図を表す歴史的資料」として扱い、変更はコードとコミットログに語らせる。既存の変更が必要になった場合は新しい Design Doc を作るか、Work Log にのみ追記する（詳細は後述）。

## 書かないこと

- **詳細な実装 How**: ループの実装方法、関数名、変数名、完全なコード実装などは、ユーザーから強い要求がない限り書かない。書くのは「何をするか」「どう責任分担するか」までで、コードそのものはコードに語らせる。
  - 参考: このリポジトリの既存 Design Doc の一部（例: `20251231_s3_connect_design.md`, `20260101_aws_integration_test_design.md`）は完全な Rust 実装コードを含んでいるが、これは本方針が定まる前に書かれた過去の慣行であり、新規作成時は踏襲しない。関数シグネチャや責務の説明までに留める。
- **チケット管理ツールの情報**: 担当者・期日・ステータスは書かない。
- **コード変更の詳細差分・全スキーマ定義**: テーブル全カラムやAPIの全フィールドは、マイグレーションファイルやAPI仕様書を正とし、Design Docには書かない。
- **実装後に判明した変更の追記更新**: Living Document 化しない。実装状況のチェックリストやテスト結果のダッシュボード（例: 「✅ 完了した機能」「テスト結果: 28/28 成功」のような節）は書かない。これは CI のテストレポートやコミットログが正とする情報であり、Design Doc に書くと陳腐化が速い。
- **承認フローや組織的なアクション記録**: 誰がいつ承認したかは書かない（ADRの役割であり、このプロジェクトでは扱わない）。

## 手順

1. **ファイル配置場所とファイル名を決める**
   - 配置先: `doc/design/`
   - 命名規則: `YYYYMMDD_<topic>_design.md`
     - `YYYYMMDD` は作成日（`date +%Y%m%d` などで取得。過去日付を偽装しない）
     - `<topic>` はテーマを表す英語スネークケース（例: `cache_key_files`, `s3_connect`, `aws_integration_test`）
     - 末尾は必ず `_design`
   - 既存の命名例を参照: `20250727_cache_key_files_design.md`, `20251231_s3_connect_design.md`, `20260101_aws_integration_test_design.md`

2. **`assets/llm_design_doc_template.md` の見出し構成をそのままコピーする**
   - 見出し番号は `1〜8`, `10〜12`（`9` は既存テンプレートに存在せず欠番。既存 Design Doc 全てがこの欠番を踏襲しているため、番号を振り直さずそのまま使う）。
   - 文書タイトル（H1）は `# <日本語での対象名> 設計ドキュメント` の形式にする。

3. **各節を以下の粒度で埋める**（既存 Design Doc 3件から見出せる規則性。冒頭「書かないこと」に反する内容は書かない）

   | 節 | 書き方 |
   |---|---|
   | 1. Overview | 最大3段落。目的と手段の要約 |
   | 2. Context | なぜ必要か、既存の課題、制約 |
   | 3. Scope | 「変更対象ファイル」「新規追加ファイル」（必要なら「変更対象外」）をテーブルで列挙し、各行にファイルパスと役割・理由を書く |
   | 4. Goal | 箇条書き。各項目に太字の見出し＋成功指標を1〜3行で |
   | 5. Non-Goal | 箇条書き。明示的に扱わない範囲を1〜3行で |
   | 6. Solution / Technical Architecture | 全体像→詳細の順で記述。Mermaid（`flowchart`/`graph`）でシステムコンテキスト図やフローを描くのは推奨。ただし完全なコード実装は書かず、責務・インターフェース・データフローの説明に留める。項目が多い場合は `6.1`, `6.2`... と細分化してよい |
   | 7. Alternative Solution | 検討した代替案ごとに `### 代替案N: 概要` の小見出しを立て、`**Pros**` / `**Cons**` / 採用可否とその理由（「判断」）を書く。ADR的な意思決定の文脈をここに書く |
   | 8. Concerns | 箇条書きで懸念事項。必要なら緩和策をネストした箇条書きで書く |
   | 10. Safety and Reliability | 「テストの実施」（ユニット/統合テストの方針）、「テストカバレッジの計測」（80%以上を目標）、「静的型付け言語の採用」の観点で書く。テストコードそのものは基本書かない |
   | 11. References | 参考にした外部ドキュメント・記事のURLを箇条書きで。各項目に簡単な説明を添える |
   | 12. Work Log | `### YYYY-MM-DD` の見出しごとに、その日行った検討・決定を箇条書きで追記する。**この節だけは、Design Doc が有効な期間（＝作業ブランチが生きている間）は追記してよい。** 日付は必ず `YYYY-MM-DD` 形式（ハイフン区切り）で統一する |

4. **ブランチの寿命を過ぎたら触らない**
   - 一度そのブランチがマージ・削除されたら、その Design Doc を更新・修正しない。実装がその後変わっても Design Doc は書き換えない。新しい設計判断が必要になったら、新しい Design Doc ファイルを起こす。
