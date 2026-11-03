//! スキャン設定（並列度・I/O 優先度・除外パターン）の型と検証。
//!
//! 設定は要求ごとに呼び出し側がすべて指定する。既定値は持たない（設計書 6.2）。
//! 範囲外の値は、型を作る時点で `Err` にする。

/// 並列度（rayon のワーカー数）。1〜64。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Parallelism(u8);

impl Parallelism {
    pub const MIN: usize = 1;
    pub const MAX: usize = 64;

    /// 範囲外（0 または 65 以上）なら `Err`。
    pub fn new(value: usize) -> anyhow::Result<Self> {
        if !(Self::MIN..=Self::MAX).contains(&value) {
            anyhow::bail!(
                "parallelism must be in {}..={}, got {}",
                Self::MIN,
                Self::MAX,
                value
            );
        }
        // MAX が 64 なので u8 に収まる
        Ok(Self(value as u8))
    }

    pub fn get(self) -> usize {
        usize::from(self.0)
    }
}

/// `best-effort` クラスのレベル。0〜7（0 が最優先）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BestEffortLevel(u8);

impl BestEffortLevel {
    pub const MAX: u8 = 7;

    /// 範囲外（8 以上）なら `Err`。
    pub fn new(level: u8) -> anyhow::Result<Self> {
        if level > Self::MAX {
            anyhow::bail!(
                "best-effort io priority level must be in 0..={}, got {}",
                Self::MAX,
                level
            );
        }
        Ok(Self(level))
    }

    pub fn get(self) -> u8 {
        self.0
    }
}

/// I/O 優先度。`realtime` は `CAP_SYS_ADMIN` が必要で他の I/O を止めうるため、
/// 型として表現しない（設計書 6.2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IoPriority {
    Idle,
    BestEffort(BestEffortLevel),
}

/// `include/uapi/linux/ioprio.h` の `IOPRIO_CLASS_SHIFT`。
const IOPRIO_CLASS_SHIFT: i32 = 13;
/// `IOPRIO_CLASS_BE`。
const IOPRIO_CLASS_BEST_EFFORT: i32 = 2;
/// `IOPRIO_CLASS_IDLE`。
const IOPRIO_CLASS_IDLE: i32 = 3;

impl IoPriority {
    /// `best-effort` のレベル 0〜7 から作る。範囲外なら `Err`。
    pub fn best_effort(level: u8) -> anyhow::Result<Self> {
        Ok(Self::BestEffort(BestEffortLevel::new(level)?))
    }

    /// `ioprio_set(2)` の `ioprio` 引数に渡す値（`IOPRIO_PRIO_VALUE(class, data)`）。
    /// `ioprio_get(2)` が返す値と比較するときにも同じ形式を使う。
    pub fn to_ioprio_value(self) -> i32 {
        match self {
            Self::Idle => IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT,
            Self::BestEffort(level) => {
                (IOPRIO_CLASS_BEST_EFFORT << IOPRIO_CLASS_SHIFT) | i32::from(level.get())
            }
        }
    }
}

impl std::fmt::Display for IoPriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "idle"),
            Self::BestEffort(level) => write!(f, "best-effort:{}", level.get()),
        }
    }
}

/// 除外パターンの一覧。コンパイル済みの照合器を持つ。
///
/// - パターンは UTF-8 の文字列で、ルートからの相対パスに対する glob。
/// - 区切りをまたがない照合（`literal_separator`）。どの階層の名前にも当てたい場合は
///   `**/.*` のように書く。
/// - 受け付けた時点で並べ替え・重複除去した正規形にする。`PartialEq`／`Eq`／`Hash` は
///   正規形のパターン文字列だけで決まる（照合器は正規形から決まるため）。
/// - 照合は相対パス全体に対して行う。ディレクトリが当たった場合に、そのサブツリーごと
///   除外できる（走査側は、ディレクトリのエントリごとに [`ExcludePatterns::is_excluded_child`]
///   を呼び、当たったら中に入らない）。
///
/// 非 UTF-8 の名前との関係（`globset` 0.4 の挙動を `tests` で確認済み）:
/// - 照合はパスのバイト列に対して行われる（Unix では `Path` の生のバイト列。UTF-8 への
///   変換で壊れない）。
/// - `*`・`**` は非 UTF-8 のバイトにも当たる（`/` 以外の任意のバイト列）。
/// - `?` は 1 バイトに当たる。非 UTF-8 のバイト 1 個には当たるが、UTF-8 の多バイト文字
///   （例: `é` は 2 バイト）には 1 個の `?` では当たらず、バイト数ぶん必要。
/// - 文字クラスの否定（`[!a]`）も 1 バイトに当たり、非 UTF-8 のバイトにも当たる。
///   肯定の文字クラス（`[a-z]`）は ASCII の範囲に書いたバイトにだけ当たる。
/// - 注意: 文字クラスは `literal_separator` の対象外で、`[!x]` や `[/]` は `/` にも当たる
///   （`a[!x]b` が `a/b` に当たる。コンパイル結果は `(?-u)^a[^x]b$`）。`?` は `[^/]` になるので
///   `/` に当たらない。区切りをまたがせたくない場合は、否定の文字クラスに `/` を含める
///   （`[!x/]`）か、`?` を使う（テスト `character_class_can_match_slash_unless_slash_is_listed`）。
/// - 非 UTF-8 の名前そのものをパターンに書くことはできない（パターンが `String`）。
#[derive(Clone, Debug)]
pub struct ExcludePatterns {
    /// 並べ替え・重複除去済み
    patterns: Vec<String>,
    matcher: globset::GlobSet,
}

impl ExcludePatterns {
    /// パターンを検証し、正規形にして照合器をコンパイルする。
    /// glob として不正なパターンがあれば `Err`。
    pub fn new(patterns: &[String]) -> anyhow::Result<Self> {
        use anyhow::Context;
        let mut normalized: Vec<String> = patterns.to_vec();
        normalized.sort();
        normalized.dedup();
        let mut set_builder = globset::GlobSetBuilder::new();
        for pattern in &normalized {
            let glob = globset::GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .with_context(|| format!("invalid exclude pattern: {pattern:?}"))?;
            set_builder.add(glob);
        }
        let matcher = set_builder
            .build()
            .context("failed to compile exclude patterns")?;
        Ok(Self {
            patterns: normalized,
            matcher,
        })
    }

    /// パターンを 1 つも持たない（何も除外しない）。
    pub fn none() -> Self {
        Self {
            patterns: Vec::new(),
            matcher: globset::GlobSet::empty(),
        }
    }

    /// 正規形（並べ替え・重複除去済み）のパターン。レポートに載せる。
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// ルートからの相対パスが除外対象か。
    /// 空のパス（ルート自身）は照合しない呼び出しを想定している。
    pub fn is_excluded(&self, relative_path: &std::path::Path) -> bool {
        self.matcher.is_match(relative_path)
    }

    /// ルートからの相対パスのバイト列（区切りは `/`）が除外対象か。
    pub fn is_excluded_bytes(&self, relative_path: &[u8]) -> bool {
        let os_str = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(relative_path);
        self.is_excluded(std::path::Path::new(os_str))
    }

    /// `parent`（ルートからの相対パス。ルート直下を走査中なら空）の下のエントリ `name` が
    /// 除外対象か。走査側が readdir の結果ごとに呼ぶ。`name` が当たらなくても、
    /// `parent` 自身が当たっていれば走査側は中に入らないので、ここでは `parent` の
    /// 祖先を個別には調べない。
    pub fn is_excluded_child(&self, parent: &std::path::Path, name: &std::ffi::OsStr) -> bool {
        if parent.as_os_str().is_empty() {
            self.is_excluded(std::path::Path::new(name))
        } else {
            self.is_excluded(&parent.join(name))
        }
    }
}

impl PartialEq for ExcludePatterns {
    fn eq(&self, other: &Self) -> bool {
        self.patterns == other.patterns
    }
}

impl Eq for ExcludePatterns {}

impl std::hash::Hash for ExcludePatterns {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.patterns.hash(state);
    }
}

/// スキャン設定の全体。統合規則（6.3）で「完全に同じ」かを `==` で比べる。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScanSettings {
    pub parallelism: Parallelism,
    pub io_priority: IoPriority,
    pub exclude_patterns: ExcludePatterns,
}

impl ScanSettings {
    pub fn new(
        parallelism: Parallelism,
        io_priority: IoPriority,
        exclude_patterns: ExcludePatterns,
    ) -> Self {
        Self {
            parallelism,
            io_priority,
            exclude_patterns,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_logger() {
        env_logger::builder().is_test(true).try_init().ok();
    }

    fn patterns(list: &[&str]) -> ExcludePatterns {
        let owned: Vec<String> = list.iter().map(|s| s.to_string()).collect();
        ExcludePatterns::new(&owned).unwrap()
    }

    fn os(bytes: &[u8]) -> &std::ffi::OsStr {
        <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(bytes)
    }

    fn hash_of<T: std::hash::Hash>(value: &T) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(value, &mut hasher);
        std::hash::Hasher::finish(&hasher)
    }

    #[test]
    fn parallelism_accepts_1_to_64() {
        init_logger();

        // Act
        let min = Parallelism::new(1).unwrap().get();
        let middle = Parallelism::new(4).unwrap().get();
        let max = Parallelism::new(64).unwrap().get();

        // Assert
        assert_eq!(min, 1);
        assert_eq!(middle, 4);
        assert_eq!(max, 64);
    }

    #[test]
    fn parallelism_rejects_out_of_range() {
        init_logger();

        // Act
        let zero = Parallelism::new(0);
        let just_over = Parallelism::new(65);
        let over = Parallelism::new(256);
        let usize_max = Parallelism::new(usize::MAX);

        // Assert
        assert!(zero.is_err());
        assert!(just_over.is_err());
        assert!(over.is_err());
        assert!(usize_max.is_err());
    }

    #[test]
    fn best_effort_level_accepts_0_to_7() {
        init_logger();

        // Act & Assert
        for level in 0..=7 {
            let actual_level = BestEffortLevel::new(level).unwrap().get();
            let priority = IoPriority::best_effort(level);
            assert_eq!(actual_level, level);
            assert!(priority.is_ok());
        }
    }

    #[test]
    fn best_effort_level_rejects_8_and_above() {
        init_logger();

        // Act
        let eight = BestEffortLevel::new(8);
        let u8_max = BestEffortLevel::new(u8::MAX);
        let priority = IoPriority::best_effort(8);

        // Assert
        assert!(eight.is_err());
        assert!(u8_max.is_err());
        assert!(priority.is_err());
    }

    #[test]
    fn ioprio_values_match_kernel_encoding() {
        init_logger();

        // Act
        let idle = IoPriority::Idle.to_ioprio_value();
        let best_effort_0 = IoPriority::best_effort(0).unwrap().to_ioprio_value();
        let best_effort_7 = IoPriority::best_effort(7).unwrap().to_ioprio_value();

        // Assert
        // IOPRIO_PRIO_VALUE(IOPRIO_CLASS_IDLE, 0) = 3 << 13
        assert_eq!(idle, 24576);
        // IOPRIO_PRIO_VALUE(IOPRIO_CLASS_BE, level) = (2 << 13) | level
        assert_eq!(best_effort_0, 16384);
        assert_eq!(best_effort_7, 16384 + 7);
    }

    #[test]
    fn io_priority_display() {
        init_logger();

        // Act
        let idle = IoPriority::Idle.to_string();
        let best_effort = IoPriority::best_effort(3).unwrap().to_string();

        // Assert
        assert_eq!(idle, "idle");
        assert_eq!(best_effort, "best-effort:3");
    }

    #[test]
    fn exclude_patterns_are_sorted_and_deduplicated() {
        init_logger();

        // Act
        let a = patterns(&["b/**", "**/.*", "b/**", "a"]);
        let b = patterns(&["a", "**/.*", "b/**"]);
        let c = patterns(&["a", "**/.*"]);
        let hash_a = hash_of(&a);
        let hash_b = hash_of(&b);

        // Assert
        assert_eq!(a.patterns(), ["**/.*", "a", "b/**"]);
        assert_eq!(a, b);
        assert_eq!(hash_a, hash_b);
        assert_ne!(a, c);
    }

    #[test]
    fn exclude_patterns_rejects_invalid_glob() {
        init_logger();

        // Act
        let unclosed_bracket = ExcludePatterns::new(&["[abc".to_string()]);
        let unclosed_brace = ExcludePatterns::new(&["{a,b".to_string()]);

        // Assert
        assert!(unclosed_bracket.is_err());
        let message = format!("{:#}", unclosed_bracket.unwrap_err());
        assert!(message.contains("[abc"), "{message}");
        assert!(unclosed_brace.is_err());
    }

    #[test]
    fn exclude_patterns_none_excludes_nothing() {
        init_logger();

        // Act
        let none = ExcludePatterns::none();
        let excluded = none.is_excluded(std::path::Path::new(".foo"));

        // Assert
        assert!(none.patterns().is_empty());
        assert!(!excluded);
        assert_eq!(none, patterns(&[]));
    }

    #[test]
    fn dot_glob_matches_dotfile_directly_under_root() {
        init_logger();

        // Arrange
        let p = patterns(&["**/.*"]);

        // Act
        let root_dotfile = p.is_excluded(std::path::Path::new(".foo"));
        let root_dotfile_child =
            p.is_excluded_child(std::path::Path::new(""), std::ffi::OsStr::new(".foo"));

        // Assert
        // ルート直下の `.foo`（`**/` は 0 階層にも当たる）
        assert!(root_dotfile);
        assert!(root_dotfile_child);

        // Act
        let deep_dotfile = p.is_excluded(std::path::Path::new("a/b/.foo"));
        let deep_dotfile_child =
            p.is_excluded_child(std::path::Path::new("a/b"), std::ffi::OsStr::new(".foo"));

        // Assert
        // 深い階層
        assert!(deep_dotfile);
        assert!(deep_dotfile_child);

        // Act
        let plain_name = p.is_excluded(std::path::Path::new("foo"));
        let inner_dot = p.is_excluded(std::path::Path::new("a/foo.bar"));
        let plain_child = p.is_excluded_child(std::path::Path::new("a"), std::ffi::OsStr::new("b"));

        // Assert
        // `.` で始まらない名前、途中の `.` は当たらない
        assert!(!plain_name);
        assert!(!inner_dot);
        assert!(!plain_child);
    }

    #[test]
    fn literal_separator_keeps_star_within_one_component() {
        init_logger();

        // Arrange
        let p = patterns(&["a/*"]);
        let q = patterns(&["*"]);

        // Act
        let direct_child = p.is_excluded(std::path::Path::new("a/b"));
        let grandchild = p.is_excluded(std::path::Path::new("a/b/c"));
        let under_other = p.is_excluded(std::path::Path::new("x/a/b"));
        let root_name = q.is_excluded(std::path::Path::new("a"));
        let nested_name = q.is_excluded(std::path::Path::new("a/b"));

        // Assert
        // `*` は `/` をまたがない（literal_separator）
        assert!(direct_child);
        assert!(!grandchild);
        assert!(!under_other);
        // `*` 単体はルート直下の名前だけ
        assert!(root_name);
        assert!(!nested_name);
    }

    #[test]
    fn directory_match_is_checked_on_each_path_prefix() {
        init_logger();

        // Arrange
        // 照合は相対パスに対して行う。走査側はディレクトリ `cache` が当たった時点で
        // 中に入らないので、`cache/x` を調べる必要はない。
        let p = patterns(&["**/cache"]);
        let q = patterns(&["**/cache/**"]);

        // Act
        let nested_child =
            p.is_excluded_child(std::path::Path::new("a"), std::ffi::OsStr::new("cache"));
        let root_dir = p.is_excluded(std::path::Path::new("cache"));
        let inside_subtree = p.is_excluded(std::path::Path::new("a/cache/x"));
        let subtree_pattern_child = q.is_excluded(std::path::Path::new("a/cache/x"));
        let subtree_pattern_grandchild = q.is_excluded(std::path::Path::new("a/cache/x/y"));

        // Assert
        assert!(nested_child);
        assert!(root_dir);
        // サブツリー内のパスそのものは `**/cache` には当たらない（中に入らないので問題ない）
        assert!(!inside_subtree);
        // サブツリーまで書くなら `**` を付ける
        assert!(subtree_pattern_child);
        assert!(subtree_pattern_grandchild);
    }

    #[test]
    fn star_matches_non_utf8_bytes() {
        init_logger();

        // Arrange
        let p = patterns(&["**/.*"]);
        let q = patterns(&["a/*"]);

        // Act
        let dot_invalid = p.is_excluded_bytes(b".\xff");
        let nested_dot_invalid = p.is_excluded_bytes(b"a/.\xff\xfe");
        let no_dot_invalid = p.is_excluded_bytes(b"a/\xff");
        let star_invalid = q.is_excluded_bytes(b"a/\xff\xfe\xfd");
        let star_across_separator = q.is_excluded_bytes(b"a/\xff/\xfe");
        let child_dot_invalid = p.is_excluded_child(std::path::Path::new("a"), os(b".\xff"));

        // Assert
        // 非 UTF-8 のバイト 0xff を含む名前
        assert!(dot_invalid);
        assert!(nested_dot_invalid);
        assert!(!no_dot_invalid);
        assert!(star_invalid);
        assert!(!star_across_separator);
        // `is_excluded_child` でも同じ
        assert!(child_dot_invalid);
    }

    #[test]
    fn star_matches_invalid_utf8_names_under_double_star() {
        init_logger();

        // Arrange
        let p = patterns(&["**/*.tmp"]);

        // Act
        let tmp = p.is_excluded_bytes(b"d/\xff\xfe.tmp");
        let txt = p.is_excluded_bytes(b"d/\xff\xfe.txt");

        // Assert
        assert!(tmp);
        assert!(!txt);
    }

    #[test]
    fn question_mark_matches_one_byte_not_one_char() {
        init_logger();

        // Arrange
        let p = patterns(&["a?"]);
        let q = patterns(&["a??"]);

        // Act
        let ascii = p.is_excluded_bytes(b"ab");
        let invalid_byte = p.is_excluded_bytes(b"a\xff");
        let multibyte_one_question = p.is_excluded_bytes("aé".as_bytes());
        let multibyte_two_questions = q.is_excluded_bytes("aé".as_bytes());
        let slash = p.is_excluded_bytes(b"a/");
        let slash_in_middle = patterns(&["a?b"]).is_excluded_bytes(b"a/b");

        // Assert
        // ASCII 1 文字
        assert!(ascii);
        // 非 UTF-8 のバイト 1 個にも当たる
        assert!(invalid_byte);
        // UTF-8 の多バイト文字 `é`（0xc3 0xa9）は 2 バイトなので、`?` 1 個には当たらない
        assert!(!multibyte_one_question);
        assert!(multibyte_two_questions);
        // `?` は `/` には当たらない（literal_separator）
        assert!(!slash);
        assert!(!slash_in_middle);
    }

    #[test]
    fn character_class_can_match_slash_unless_slash_is_listed() {
        init_logger();

        // Arrange
        // 文字クラスは globset の `literal_separator` の対象外で、`/` に当たる。
        // これを仕様として固定する。globset の更新で挙動が変わったときに気づくためのテスト。
        let negated = patterns(&["a[!x]b"]);
        let negated_with_slash = patterns(&["a[!x/]b"]);
        let slash_class = patterns(&["a[/]b"]);

        // Act
        let negated_matches = negated.is_excluded_bytes(b"a/b");
        let negated_with_slash_matches = negated_with_slash.is_excluded_bytes(b"a/b");
        let slash_class_matches = slash_class.is_excluded_bytes(b"a/b");

        // Assert
        // 否定の文字クラスは `/` に当たる
        assert!(negated_matches);
        // `/` を含めた否定の文字クラスは `/` に当たらない
        assert!(!negated_with_slash_matches);
        // `/` だけの文字クラスは `/` に当たる
        assert!(slash_class_matches);
    }

    #[test]
    fn negated_class_matches_non_utf8_byte_but_positive_class_does_not() {
        init_logger();

        // Arrange
        let neg = patterns(&["a[!x]"]);
        let pos = patterns(&["a[a-z]"]);

        // Act
        let neg_ascii = neg.is_excluded_bytes(b"ab");
        let neg_invalid_byte = neg.is_excluded_bytes(b"a\xff");
        let neg_excluded_char = neg.is_excluded_bytes(b"ax");

        // Assert
        // 否定の文字クラスは任意の 1 バイト（`/` 以外）に当たる。非 UTF-8 のバイトにも当たる
        assert!(neg_ascii);
        assert!(neg_invalid_byte);
        assert!(!neg_excluded_char);

        // Act
        let pos_in_range = pos.is_excluded_bytes(b"ab");
        let pos_invalid_byte = pos.is_excluded_bytes(b"a\xff");
        let pos_out_of_range = pos.is_excluded_bytes(b"aB");

        // Assert
        // 肯定の文字クラスは書いた範囲のバイトにだけ当たる
        assert!(pos_in_range);
        assert!(!pos_invalid_byte);
        assert!(!pos_out_of_range);

        // Act
        let neg_slash = neg.is_excluded_bytes(b"a/");
        let neg_slash_in_middle = patterns(&["a[!x]b"]).is_excluded_bytes(b"a/b");
        let pos_slash_in_middle = patterns(&["a[/]b"]).is_excluded_bytes(b"a/b");

        // Assert
        // 注意: 文字クラスは literal_separator の対象外で、`/` にも当たる（`?` や `*` と違う）
        assert!(neg_slash);
        assert!(neg_slash_in_middle);
        assert!(pos_slash_in_middle);

        // Act
        let neg_with_slash_across = patterns(&["a[!x/]b"]).is_excluded_bytes(b"a/b");
        let neg_with_slash_invalid_byte = patterns(&["a[!x/]b"]).is_excluded_bytes(b"a\xffb");

        // Assert
        // `/` を含めて否定すれば区切りをまたがない
        assert!(!neg_with_slash_across);
        assert!(neg_with_slash_invalid_byte);
    }

    #[test]
    fn pattern_cannot_name_non_utf8_but_star_can_reach_it() {
        init_logger();

        // Arrange
        // `\xff` を含む名前そのものは書けない。リテラルの `\xff` に見える文字列は
        // UTF-8 の通常の文字列として扱われ、バイト 0xff には当たらない
        let p = patterns(&["a/\\xff"]);

        // Act
        let literal = p.is_excluded_bytes(b"a/\xff");
        let wildcard = patterns(&["a/*"]).is_excluded_bytes(b"a/\xff");

        // Assert
        assert!(!literal);
        // ワイルドカードなら対象にできる
        assert!(wildcard);
    }

    #[test]
    fn settings_equality_covers_all_fields() {
        init_logger();

        // Arrange
        let base = ScanSettings::new(
            Parallelism::new(4).unwrap(),
            IoPriority::Idle,
            patterns(&["**/.*", "x"]),
        );
        let same = ScanSettings::new(
            Parallelism::new(4).unwrap(),
            IoPriority::Idle,
            patterns(&["x", "**/.*", "x"]),
        );
        let other_parallelism = ScanSettings::new(
            Parallelism::new(5).unwrap(),
            IoPriority::Idle,
            patterns(&["**/.*", "x"]),
        );
        let other_priority = ScanSettings::new(
            Parallelism::new(4).unwrap(),
            IoPriority::best_effort(4).unwrap(),
            patterns(&["**/.*", "x"]),
        );
        let other_best_effort_level = ScanSettings::new(
            Parallelism::new(4).unwrap(),
            IoPriority::best_effort(5).unwrap(),
            patterns(&["**/.*", "x"]),
        );
        let other_patterns = ScanSettings::new(
            Parallelism::new(4).unwrap(),
            IoPriority::Idle,
            patterns(&["**/.*"]),
        );

        // Act
        let hash_base = hash_of(&base);
        let hash_same = hash_of(&same);
        let cloned = base.clone();

        // Assert
        assert_eq!(base, same);
        assert_eq!(hash_base, hash_same);
        assert_eq!(cloned, base);
        assert_ne!(base, other_parallelism);
        assert_ne!(base, other_priority);
        assert_ne!(other_priority, other_best_effort_level);
        assert_ne!(base, other_patterns);
    }
}
