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
///   （`[!x/]`）か、`?` を使う。
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
        assert_eq!(Parallelism::new(1).unwrap().get(), 1);
        assert_eq!(Parallelism::new(4).unwrap().get(), 4);
        assert_eq!(Parallelism::new(64).unwrap().get(), 64);
    }

    #[test]
    fn parallelism_rejects_out_of_range() {
        init_logger();
        assert!(Parallelism::new(0).is_err());
        assert!(Parallelism::new(65).is_err());
        assert!(Parallelism::new(256).is_err());
        assert!(Parallelism::new(usize::MAX).is_err());
    }

    #[test]
    fn best_effort_level_accepts_0_to_7() {
        init_logger();
        for level in 0..=7 {
            assert_eq!(BestEffortLevel::new(level).unwrap().get(), level);
            assert!(IoPriority::best_effort(level).is_ok());
        }
    }

    #[test]
    fn best_effort_level_rejects_8_and_above() {
        init_logger();
        assert!(BestEffortLevel::new(8).is_err());
        assert!(BestEffortLevel::new(u8::MAX).is_err());
        assert!(IoPriority::best_effort(8).is_err());
    }

    #[test]
    fn ioprio_values_match_kernel_encoding() {
        init_logger();
        // IOPRIO_PRIO_VALUE(IOPRIO_CLASS_IDLE, 0) = 3 << 13
        assert_eq!(IoPriority::Idle.to_ioprio_value(), 24576);
        // IOPRIO_PRIO_VALUE(IOPRIO_CLASS_BE, level) = (2 << 13) | level
        assert_eq!(IoPriority::best_effort(0).unwrap().to_ioprio_value(), 16384);
        assert_eq!(
            IoPriority::best_effort(7).unwrap().to_ioprio_value(),
            16384 + 7
        );
    }

    #[test]
    fn io_priority_display() {
        init_logger();
        assert_eq!(IoPriority::Idle.to_string(), "idle");
        assert_eq!(
            IoPriority::best_effort(3).unwrap().to_string(),
            "best-effort:3"
        );
    }

    #[test]
    fn exclude_patterns_are_sorted_and_deduplicated() {
        init_logger();
        let a = patterns(&["b/**", "**/.*", "b/**", "a"]);
        assert_eq!(a.patterns(), ["**/.*", "a", "b/**"]);
        let b = patterns(&["a", "**/.*", "b/**"]);
        assert_eq!(a, b);
        assert_eq!(hash_of(&a), hash_of(&b));
        let c = patterns(&["a", "**/.*"]);
        assert_ne!(a, c);
    }

    #[test]
    fn exclude_patterns_rejects_invalid_glob() {
        init_logger();
        let result = ExcludePatterns::new(&["[abc".to_string()]);
        assert!(result.is_err());
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("[abc"), "{message}");
        assert!(ExcludePatterns::new(&["{a,b".to_string()]).is_err());
    }

    #[test]
    fn exclude_patterns_none_excludes_nothing() {
        init_logger();
        let none = ExcludePatterns::none();
        assert!(none.patterns().is_empty());
        assert!(!none.is_excluded(std::path::Path::new(".foo")));
        assert_eq!(none, patterns(&[]));
    }

    #[test]
    fn dot_glob_matches_dotfile_directly_under_root() {
        init_logger();
        let p = patterns(&["**/.*"]);
        // ルート直下の `.foo`（`**/` は 0 階層にも当たる）
        assert!(p.is_excluded(std::path::Path::new(".foo")));
        assert!(p.is_excluded_child(std::path::Path::new(""), std::ffi::OsStr::new(".foo")));
        // 深い階層
        assert!(p.is_excluded(std::path::Path::new("a/b/.foo")));
        assert!(p.is_excluded_child(std::path::Path::new("a/b"), std::ffi::OsStr::new(".foo")));
        // `.` で始まらない名前、途中の `.` は当たらない
        assert!(!p.is_excluded(std::path::Path::new("foo")));
        assert!(!p.is_excluded(std::path::Path::new("a/foo.bar")));
        assert!(!p.is_excluded_child(std::path::Path::new("a"), std::ffi::OsStr::new("b")));
    }

    #[test]
    fn literal_separator_keeps_star_within_one_component() {
        init_logger();
        // `*` は `/` をまたがない（literal_separator）
        let p = patterns(&["a/*"]);
        assert!(p.is_excluded(std::path::Path::new("a/b")));
        assert!(!p.is_excluded(std::path::Path::new("a/b/c")));
        assert!(!p.is_excluded(std::path::Path::new("x/a/b")));
        // `*` 単体はルート直下の名前だけ
        let q = patterns(&["*"]);
        assert!(q.is_excluded(std::path::Path::new("a")));
        assert!(!q.is_excluded(std::path::Path::new("a/b")));
    }

    #[test]
    fn directory_match_is_checked_on_each_path_prefix() {
        init_logger();
        // 照合は相対パスに対して行う。走査側はディレクトリ `cache` が当たった時点で
        // 中に入らないので、`cache/x` を調べる必要はない。
        let p = patterns(&["**/cache"]);
        assert!(p.is_excluded_child(std::path::Path::new("a"), std::ffi::OsStr::new("cache")));
        assert!(p.is_excluded(std::path::Path::new("cache")));
        // サブツリー内のパスそのものは `**/cache` には当たらない（中に入らないので問題ない）
        assert!(!p.is_excluded(std::path::Path::new("a/cache/x")));
        // サブツリーまで書くなら `**` を付ける
        let q = patterns(&["**/cache/**"]);
        assert!(q.is_excluded(std::path::Path::new("a/cache/x")));
        assert!(q.is_excluded(std::path::Path::new("a/cache/x/y")));
    }

    #[test]
    fn star_matches_non_utf8_bytes() {
        init_logger();
        let p = patterns(&["**/.*"]);
        // 非 UTF-8 のバイト 0xff を含む名前
        assert!(p.is_excluded_bytes(b".\xff"));
        assert!(p.is_excluded_bytes(b"a/.\xff\xfe"));
        assert!(!p.is_excluded_bytes(b"a/\xff"));
        let q = patterns(&["a/*"]);
        assert!(q.is_excluded_bytes(b"a/\xff\xfe\xfd"));
        assert!(!q.is_excluded_bytes(b"a/\xff/\xfe"));
        // `is_excluded_child` でも同じ
        assert!(p.is_excluded_child(std::path::Path::new("a"), os(b".\xff")));
    }

    #[test]
    fn star_matches_invalid_utf8_names_under_double_star() {
        init_logger();
        let p = patterns(&["**/*.tmp"]);
        assert!(p.is_excluded_bytes(b"d/\xff\xfe.tmp"));
        assert!(!p.is_excluded_bytes(b"d/\xff\xfe.txt"));
    }

    #[test]
    fn question_mark_matches_one_byte_not_one_char() {
        init_logger();
        let p = patterns(&["a?"]);
        // ASCII 1 文字
        assert!(p.is_excluded_bytes(b"ab"));
        // 非 UTF-8 のバイト 1 個にも当たる
        assert!(p.is_excluded_bytes(b"a\xff"));
        // UTF-8 の多バイト文字 `é`（0xc3 0xa9）は 2 バイトなので、`?` 1 個には当たらない
        assert!(!p.is_excluded_bytes("aé".as_bytes()));
        let q = patterns(&["a??"]);
        assert!(q.is_excluded_bytes("aé".as_bytes()));
        // `?` は `/` には当たらない（literal_separator）
        assert!(!p.is_excluded_bytes(b"a/"));
        assert!(!patterns(&["a?b"]).is_excluded_bytes(b"a/b"));
    }

    #[test]
    fn negated_class_matches_non_utf8_byte_but_positive_class_does_not() {
        init_logger();
        // 否定の文字クラスは任意の 1 バイト（`/` 以外）に当たる。非 UTF-8 のバイトにも当たる
        let neg = patterns(&["a[!x]"]);
        assert!(neg.is_excluded_bytes(b"ab"));
        assert!(neg.is_excluded_bytes(b"a\xff"));
        assert!(!neg.is_excluded_bytes(b"ax"));
        // 肯定の文字クラスは、書いた範囲のバイトにだけ当たる
        let pos = patterns(&["a[a-z]"]);
        assert!(pos.is_excluded_bytes(b"ab"));
        assert!(!pos.is_excluded_bytes(b"a\xff"));
        assert!(!pos.is_excluded_bytes(b"aB"));
        // 注意: 文字クラスは literal_separator の対象外で、`/` にも当たる（`?` や `*` と違う）
        assert!(neg.is_excluded_bytes(b"a/"));
        assert!(patterns(&["a[!x]b"]).is_excluded_bytes(b"a/b"));
        assert!(patterns(&["a[/]b"]).is_excluded_bytes(b"a/b"));
        // `/` を含めて否定すれば区切りをまたがない
        assert!(!patterns(&["a[!x/]b"]).is_excluded_bytes(b"a/b"));
        assert!(patterns(&["a[!x/]b"]).is_excluded_bytes(b"a\xffb"));
    }

    #[test]
    fn pattern_cannot_name_non_utf8_but_star_can_reach_it() {
        init_logger();
        // `\xff` を含む名前そのものは書けない。リテラルの `\xff` に見える文字列は
        // UTF-8 の通常の文字列として扱われ、バイト 0xff には当たらない
        let p = patterns(&["a/\\xff"]);
        assert!(!p.is_excluded_bytes(b"a/\xff"));
        // ワイルドカードなら対象にできる
        assert!(patterns(&["a/*"]).is_excluded_bytes(b"a/\xff"));
    }

    #[test]
    fn settings_equality_covers_all_fields() {
        init_logger();
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
        assert_eq!(base, same);
        assert_eq!(hash_of(&base), hash_of(&same));
        assert_eq!(base.clone(), base);

        let other_parallelism = ScanSettings::new(
            Parallelism::new(5).unwrap(),
            IoPriority::Idle,
            patterns(&["**/.*", "x"]),
        );
        assert_ne!(base, other_parallelism);
        let other_priority = ScanSettings::new(
            Parallelism::new(4).unwrap(),
            IoPriority::best_effort(4).unwrap(),
            patterns(&["**/.*", "x"]),
        );
        assert_ne!(base, other_priority);
        let other_best_effort_level = ScanSettings::new(
            Parallelism::new(4).unwrap(),
            IoPriority::best_effort(5).unwrap(),
            patterns(&["**/.*", "x"]),
        );
        assert_ne!(other_priority, other_best_effort_level);
        let other_patterns = ScanSettings::new(
            Parallelism::new(4).unwrap(),
            IoPriority::Idle,
            patterns(&["**/.*"]),
        );
        assert_ne!(base, other_patterns);
    }
}
