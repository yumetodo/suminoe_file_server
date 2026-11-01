//! メモリ上のツリー（データモデル）と、スキャン結果の反映・サイズの伝播・状態の管理
//! （設計書 6.4, 6.6）。
//!
//! このモジュールはファイルシステムに触れない。走査側（`walk`）が [`DirNode`] の部分木を
//! 組み立て、サービス側が [`Tree`] に反映する。ロック（`std::sync::RwLock`）はサービス側が
//! 持つので、ここの型は素の構造体として定義する。

/// ツリーの操作が返すエラー。
///
/// [`TreeError::is_fatal`] が `true` のものは、ツリーの内部不整合など回復不能なエラーで、
/// 呼び出し側（スキャンサービス）は `std::process::abort()` でプロセスを終了する
/// （設計書 6.6）。このモジュール自身は `abort` しない（テストで検出できるようにするため）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeError {
    /// 内部不整合。祖先の合計が子の合計より小さい（`checked_sub` の失敗）、合計の加算が
    /// 溢れた、呼び出しの前提（部分木の名前と位置が一致する、など）が崩れている
    Inconsistent(String),
    /// `generation` が `u64` の上限に達して増やせない
    GenerationOverflow,
    /// 指定されたパスがツリーにない（ディレクトリとして載っていない）
    PathNotFound(std::path::PathBuf),
    /// ツリーが未スキャンのため、部分木を反映できない
    NotScanned,
    /// 同じ名前のファイルまたはディレクトリを 2 回追加した（readdir の結果に同じ名前は
    /// 2 つ出ないので、呼び出し側の不具合）
    DuplicateName(std::ffi::OsString),
}

impl TreeError {
    /// 回復不能なエラーか。`true` ならサービスはプロセスを終了する。
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Inconsistent(_) | Self::GenerationOverflow)
    }
}

impl std::fmt::Display for TreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inconsistent(reason) => write!(f, "tree is inconsistent: {reason}"),
            Self::GenerationOverflow => f.write_str("generation overflowed"),
            Self::PathNotFound(path) => write!(
                f,
                "directory is not in the tree: {}",
                crate::dir_scan::report::escape_path_for_log(path)
            ),
            Self::NotScanned => f.write_str("tree is not scanned yet"),
            Self::DuplicateName(name) => write!(
                f,
                "duplicate name: {}",
                crate::dir_scan::report::escape_os_str_for_log(name)
            ),
        }
    }
}

impl std::error::Error for TreeError {}

/// ファイル名のハッシュ生成器（設計書 6.4）。
///
/// std の `RandomState`（ランダムなシード）を持つ具体的な型で、型パラメータにはしない。
/// 同じ値の間は、同じ名前のバイト列から常に同じ値を返す。`Clone` した複製も同じ値を返す
/// （部分スキャンは開始時に複製して、走査から反映まで同じものを使う）。
#[derive(Clone, Debug)]
pub struct NameHasher {
    random_state: std::hash::RandomState,
    /// テストで衝突を起こすための、名前のバイト列からハッシュへの写像
    #[cfg(test)]
    test_mapping: Option<fn(&[u8]) -> u64>,
}

impl NameHasher {
    /// ランダムなシードの生成器を作る。
    pub fn new() -> Self {
        Self {
            random_state: std::hash::RandomState::new(),
            #[cfg(test)]
            test_mapping: None,
        }
    }

    /// テスト用。`mapping` が返す値をそのままハッシュとして返す生成器を作る
    /// （衝突させるため）。
    #[cfg(test)]
    pub fn with_test_mapping(mapping: fn(&[u8]) -> u64) -> Self {
        Self {
            random_state: std::hash::RandomState::new(),
            test_mapping: Some(mapping),
        }
    }

    /// 名前のバイト列（表示用の文字列ではなく元のバイト列）のハッシュ。
    pub fn hash_name(&self, name: &std::ffi::OsStr) -> u64 {
        let bytes = std::os::unix::ffi::OsStrExt::as_bytes(name);
        #[cfg(test)]
        {
            if let Some(mapping) = self.test_mapping {
                return mapping(bytes);
            }
        }
        std::hash::BuildHasher::hash_one(&self.random_state, bytes)
    }
}

impl Default for NameHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// ハッシュが衝突したファイル 1 件（実名とサイズ）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollidedFile {
    pub name: std::ffi::OsString,
    pub size: u64,
}

/// 名前のハッシュで引くファイルのエントリ（設計書 6.4）。
///
/// 通常は単独で、サイズだけを持つ（`Box<[_]>` にして、列挙型全体を 16 バイトに収める）。同じディレクトリ内でハッシュが重なったときだけ
/// 「衝突」にして、実名とサイズの一覧で持つ。衝突中のファイルも実名で引けるので、
/// 削除やサイズ変更を正しく反映できる。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileEntry {
    /// 単独（ファイルのサイズ `st_size`）
    Single(u64),
    /// 衝突（2 件以上の、実名とサイズの一覧）
    Collided(Box<[CollidedFile]>),
}

impl FileEntry {
    /// このエントリが表すファイルの数。
    pub fn file_count(&self) -> u64 {
        match self {
            Self::Single(_) => 1,
            Self::Collided(files) => files.len() as u64,
        }
    }

    /// このエントリが表すファイルのサイズの合計。
    /// `u64` を `u128` に足すので、現実的なファイル数では溢れない。溢れたら `None`。
    pub fn total_size(&self) -> Option<u128> {
        match self {
            Self::Single(size) => Some(u128::from(*size)),
            Self::Collided(files) => {
                let mut total: u128 = 0;
                for file in files {
                    total = total.checked_add(u128::from(file.size))?;
                }
                Some(total)
            }
        }
    }
}

/// ディレクトリ自身のエラーの集約（設計書 6.4）。個々のエラーの詳細は残さない。
///
/// `ENOENT`（readdir と stat の間にファイルが消えた）は正常な競合として、ここには記録しない。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirErrors {
    /// ディレクトリを開けなかった場合のエラー番号
    pub open_errno: Option<i32>,
    /// readdir が途中で失敗した場合のエラー番号
    pub read_dir_errno: Option<i32>,
    /// ファイルの stat が `EACCES` で失敗した件数
    pub stat_eacces: u32,
    /// ファイルの stat が `EIO` で失敗した件数
    pub stat_eio: u32,
    /// ファイルの stat が `EACCES`・`EIO`・`ENOENT` 以外で失敗した件数
    pub stat_other: u32,
    /// 循環を検出した
    pub cycle_detected: bool,
}

impl DirErrors {
    /// 何かエラーを持つか。
    pub fn has_error(&self) -> bool {
        self.open_errno.is_some()
            || self.read_dir_errno.is_some()
            || self.stat_eacces > 0
            || self.stat_eio > 0
            || self.stat_other > 0
            || self.cycle_detected
    }
}

/// ツリー内のディレクトリの数・ファイルの数・ハッシュ衝突の件数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeCounts {
    /// ディレクトリの数（数え始めたノード自身を含む）
    pub dir_count: u64,
    /// ファイルの数
    pub file_count: u64,
    /// ハッシュ衝突の件数。衝突を検出した追加（既に同じハッシュのファイルがあったもの）の数
    /// で、ファイルの数からハッシュ表のエントリ数を引いた値に等しい
    pub hash_collision_count: u64,
}

/// ディレクトリのノード（設計書 6.4）。
///
/// 子ディレクトリは「名前 → ノード」の表で直接所有する。親への参照は持たない。
/// 走査側は、`new` で作ったノードに [`DirNode::set_files`]・[`DirNode::add_child`]・
/// `record_*` を呼び、子を先に完成させてから [`DirNode::finalize`] で合計を集計する
/// （子から親へ、下から順に行う）。
#[derive(Debug, PartialEq)]
pub struct DirNode {
    name: std::ffi::OsString,
    dev: u64,
    ino: u64,
    own_size: u128,
    total_size: u128,
    errors: DirErrors,
    subtree_error_dir_count: u64,
    children: std::collections::HashMap<std::ffi::OsString, DirNode>,
    files: std::collections::HashMap<u64, FileEntry>,
}

impl DirNode {
    /// 空のノードを作る。`name` はディレクトリ名（ルートなら空でよい）。
    /// 合計は 0、エラーなし。
    pub fn new(name: std::ffi::OsString, dev: u64, ino: u64) -> Self {
        Self {
            name,
            dev,
            ino,
            own_size: 0,
            total_size: 0,
            errors: DirErrors::default(),
            subtree_error_dir_count: 0,
            children: std::collections::HashMap::new(),
            files: std::collections::HashMap::new(),
        }
    }

    pub fn name(&self) -> &std::ffi::OsStr {
        &self.name
    }

    pub fn dev(&self) -> u64 {
        self.dev
    }

    pub fn ino(&self) -> u64 {
        self.ino
    }

    /// 直下のファイルの合計。[`DirNode::finalize`] の後に正しい値になる。
    pub fn own_size(&self) -> u128 {
        self.own_size
    }

    /// 配下すべての合計。[`DirNode::finalize`] の後に正しい値になる。
    pub fn total_size(&self) -> u128 {
        self.total_size
    }

    /// このディレクトリ自身のエラーの集約。
    pub fn errors(&self) -> &DirErrors {
        &self.errors
    }

    /// 自身を含む配下でエラーを持つディレクトリの数。[`DirNode::finalize`] の後に正しい値に
    /// なる。0 なら、このノードの合計は「完全」。
    pub fn subtree_error_dir_count(&self) -> u64 {
        self.subtree_error_dir_count
    }

    /// このノードの合計が完全か（配下にエラーを持つディレクトリがないか）。
    pub fn is_complete(&self) -> bool {
        self.subtree_error_dir_count == 0
    }

    pub fn children(&self) -> &std::collections::HashMap<std::ffi::OsString, DirNode> {
        &self.children
    }

    pub fn child(&self, name: &std::ffi::OsStr) -> Option<&DirNode> {
        self.children.get(name)
    }

    pub fn files(&self) -> &std::collections::HashMap<u64, FileEntry> {
        &self.files
    }

    /// 直下のファイルをまとめて設定する。`files` は readdir と stat の結果の（実名, サイズ）。
    ///
    /// 名前のハッシュが重なったものは、確実に衝突なので、そのハッシュのエントリだけを
    /// [`FileEntry::Collided`] にして実名で持つ（設計書 6.4）。衝突の件数
    /// （[`TreeCounts::hash_collision_count`] の定義と同じ）を返す。
    /// 同じ名前が 2 つあれば [`TreeError::DuplicateName`]、既にファイルがあれば
    /// [`TreeError::Inconsistent`]。失敗したときは何も変更しない。
    ///
    /// 実名は、衝突したときのためにここで全件を一度に受け取る（単独のエントリは実名を
    /// 持たないので、後から 1 件ずつ追加すると、先に入れたファイルの実名が分からない）。
    pub fn set_files(
        &mut self,
        hasher: &NameHasher,
        files: &[(std::ffi::OsString, u64)],
    ) -> Result<u64, TreeError> {
        if !self.files.is_empty() {
            return Err(TreeError::Inconsistent(
                "files of the directory are already set".to_string(),
            ));
        }
        let mut hashed: Vec<(u64, usize)> = files
            .iter()
            .enumerate()
            .map(|(index, (name, _size))| (hasher.hash_name(name), index))
            .collect();
        hashed.sort_unstable();
        // 同じハッシュの範囲（hashed は並べ替え済み）
        let mut groups: Vec<std::ops::Range<usize>> = Vec::new();
        let mut group_start: usize = 0;
        while group_start < hashed.len() {
            let hash = hashed[group_start].0;
            let mut group_end = group_start + 1;
            while group_end < hashed.len() && hashed[group_end].0 == hash {
                group_end += 1;
            }
            groups.push(group_start..group_end);
            group_start = group_end;
        }
        // 先に重複した名前を調べて、失敗したときに何も変更しないようにする
        for range in &groups {
            let group = &hashed[range.clone()];
            for (position, (_, index)) in group.iter().enumerate() {
                for (_, other_index) in &group[position + 1..] {
                    if files[*index].0 == files[*other_index].0 {
                        return Err(TreeError::DuplicateName(files[*index].0.clone()));
                    }
                }
            }
        }
        let mut table: std::collections::HashMap<u64, FileEntry> =
            std::collections::HashMap::with_capacity(groups.len());
        let mut collision_count: u64 = 0;
        for range in &groups {
            let group = &hashed[range.clone()];
            let hash = group[0].0;
            if group.len() == 1 {
                table.insert(hash, FileEntry::Single(files[group[0].1].1));
            } else {
                let collided: Box<[CollidedFile]> = group
                    .iter()
                    .map(|(_, index)| CollidedFile {
                        name: files[*index].0.clone(),
                        size: files[*index].1,
                    })
                    .collect();
                collision_count += (group.len() - 1) as u64;
                table.insert(hash, FileEntry::Collided(collided));
            }
        }
        self.files = table;
        Ok(collision_count)
    }

    /// 子ディレクトリを追加する。子は [`DirNode::finalize`] 済みであること。
    /// 同じ名前の子があれば [`TreeError::DuplicateName`]。
    pub fn add_child(&mut self, child: DirNode) -> Result<(), TreeError> {
        match self.children.entry(child.name.clone()) {
            std::collections::hash_map::Entry::Occupied(_) => {
                Err(TreeError::DuplicateName(child.name))
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(child);
                Ok(())
            }
        }
    }

    /// ディレクトリを開けなかったエラーを付ける。
    pub fn record_open_error(&mut self, errno: i32) {
        self.errors.open_errno = Some(errno);
    }

    /// readdir が途中で失敗したエラーを付ける。
    pub fn record_read_dir_error(&mut self, errno: i32) {
        self.errors.read_dir_errno = Some(errno);
    }

    /// ファイルの stat の失敗を付ける。`ENOENT` は正常な競合なので記録しない。
    /// 件数は飽和加算する（数えるだけの値なので、溢れても内部不整合にはしない）。
    pub fn record_stat_error(&mut self, errno: i32) {
        if errno == libc::ENOENT {
            return;
        }
        let counter = if errno == libc::EACCES {
            &mut self.errors.stat_eacces
        } else if errno == libc::EIO {
            &mut self.errors.stat_eio
        } else {
            &mut self.errors.stat_other
        };
        *counter = counter.saturating_add(1);
    }

    /// 循環の検出を付ける。
    pub fn record_cycle(&mut self) {
        self.errors.cycle_detected = true;
    }

    /// 子（集計済み）から `own_size`・`total_size`・`subtree_error_dir_count` を集計する。
    /// 加算は `checked_add` で行い、溢れは [`TreeError::Inconsistent`]。
    /// 失敗したときは何も変更しない。
    pub fn finalize(&mut self) -> Result<(), TreeError> {
        let mut own_size: u128 = 0;
        for entry in self.files.values() {
            let entry_size = entry.total_size().ok_or_else(|| {
                TreeError::Inconsistent("sum of file sizes of an entry overflowed".to_string())
            })?;
            own_size = own_size
                .checked_add(entry_size)
                .ok_or_else(|| TreeError::Inconsistent("own_size overflowed".to_string()))?;
        }
        let mut total_size = own_size;
        let mut error_dir_count: u64 = u64::from(self.errors.has_error());
        for child in self.children.values() {
            total_size = total_size
                .checked_add(child.total_size)
                .ok_or_else(|| TreeError::Inconsistent("total_size overflowed".to_string()))?;
            error_dir_count = error_dir_count
                .checked_add(child.subtree_error_dir_count)
                .ok_or_else(|| {
                    TreeError::Inconsistent("subtree_error_dir_count overflowed".to_string())
                })?;
        }
        self.own_size = own_size;
        self.total_size = total_size;
        self.subtree_error_dir_count = error_dir_count;
        Ok(())
    }

    /// このノードを根とする部分木の、ディレクトリ数・ファイル数・ハッシュ衝突の件数。
    /// 深い木でスタックを使い切らないよう、明示的なスタックで数える。
    pub fn counts(&self) -> TreeCounts {
        let mut counts = TreeCounts::default();
        let mut pending: Vec<&DirNode> = vec![self];
        while let Some(node) = pending.pop() {
            counts.dir_count += 1;
            let mut node_file_count: u64 = 0;
            for entry in node.files.values() {
                node_file_count += entry.file_count();
            }
            counts.file_count += node_file_count;
            counts.hash_collision_count += node_file_count - node.files.len() as u64;
            pending.extend(node.children.values());
        }
        counts
    }
}

/// 公開しているツリーの状態（設計書 6.4）。ルートの `subtree_error_dir_count` から決まる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotState {
    /// まだフルスキャンが成功していない
    Unscanned,
    /// 完全（エラーを持つディレクトリがない）
    Complete,
    /// 不完全（エラーを持つディレクトリがある）
    Incomplete,
}

/// 直近のスキャンの結果と終了時刻。直近のスキャンが失敗しても、前のツリーは公開し続ける。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastScan {
    /// 成功／エラーありで成功／失敗
    pub outcome: crate::dir_scan::report::ScanOutcome,
    pub finished_at: std::time::SystemTime,
}

/// ツリーの状態（読み取り用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeStatus {
    pub snapshot: SnapshotState,
    /// まだ 1 回もスキャンが終わっていなければ `None`
    pub last_scan: Option<LastScan>,
    pub generation: u64,
}

/// フルスキャンが組み立てた、公開前のツリー。
pub struct FullScanTree {
    pub root: DirNode,
    /// このフルスキャンの除外パターン（正規形）
    pub exclude_patterns: crate::dir_scan::settings::ExcludePatterns,
    /// このフルスキャンで使った（`root` のハッシュを計算した）ハッシュ生成器
    pub name_hasher: NameHasher,
}

/// フルスキャンの反映で外した旧ツリー。書き込みロックを手放してから `drop` する。
pub struct RetiredFullTree {
    /// 旧ルート（未スキャンだったら `None`）
    pub root: Option<DirNode>,
    pub name_hasher: NameHasher,
    pub exclude_patterns: Option<crate::dir_scan::settings::ExcludePatterns>,
}

/// フルスキャンの反映の結果。
pub struct ApplyFullOutcome {
    pub generation_before: u64,
    pub generation_after: u64,
    pub retired: RetiredFullTree,
}

/// 部分スキャンの反映の結果。
pub struct ApplyPartialOutcome {
    pub generation_before: u64,
    pub generation_after: u64,
    /// 外した旧部分木。書き込みロックを手放してから `drop` する
    pub retired_subtree: DirNode,
}

/// 部分スキャンの要求パスを、ツリーで辿った結果（設計書 6.6 の表）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedTarget {
    /// 実際に走査する対象（ルートからの相対パス。ルートなら空）
    pub effective_path: std::path::PathBuf,
    /// 要求のパス全体がディレクトリとしてツリーにあった（`effective_path` が要求のパスと
    /// 同じ）か
    pub exact: bool,
}

/// 公開するツリー（スキャン状態を含む）。素の構造体で、`std::sync::RwLock` で包むのは
/// サービス側。
///
/// ツリーを書き換える操作は `&mut self` の [`Tree::apply_full`]・[`Tree::apply_partial`]・
/// [`Tree::record_failed_scan`] だけで、どれも失敗したときは何も変更しない。
pub struct Tree {
    root: Option<DirNode>,
    generation: u64,
    last_scan: Option<LastScan>,
    exclude_patterns: Option<crate::dir_scan::settings::ExcludePatterns>,
    name_hasher: NameHasher,
}

impl Tree {
    /// 未スキャンのツリーを作る（`generation` は 0）。
    pub fn new() -> Self {
        Self {
            root: None,
            generation: 0,
            last_scan: None,
            exclude_patterns: None,
            name_hasher: NameHasher::new(),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 公開しているツリーの状態。ルートの `subtree_error_dir_count` から決める。
    pub fn snapshot_state(&self) -> SnapshotState {
        match &self.root {
            None => SnapshotState::Unscanned,
            Some(root) => {
                if root.is_complete() {
                    SnapshotState::Complete
                } else {
                    SnapshotState::Incomplete
                }
            }
        }
    }

    pub fn last_scan(&self) -> Option<&LastScan> {
        self.last_scan.as_ref()
    }

    pub fn status(&self) -> TreeStatus {
        TreeStatus {
            snapshot: self.snapshot_state(),
            last_scan: self.last_scan.clone(),
            generation: self.generation,
        }
    }

    /// ツリーを作ったフルスキャンの除外パターン。未スキャンなら `None`。
    /// [`crate::dir_scan::request::needs_full_upgrade`] と
    /// [`crate::dir_scan::request::effective_target`] にそのまま渡せる。
    pub fn exclude_patterns(&self) -> Option<&crate::dir_scan::settings::ExcludePatterns> {
        self.exclude_patterns.as_ref()
    }

    /// ツリーのハッシュ生成器。部分スキャンは開始時に複製して、走査から反映まで同じものを
    /// 使う。
    pub fn name_hasher(&self) -> &NameHasher {
        &self.name_hasher
    }

    pub fn root(&self) -> Option<&DirNode> {
        self.root.as_ref()
    }

    /// ルートの合計サイズ。未スキャンなら `None`。
    pub fn root_total_size(&self) -> Option<u128> {
        self.root.as_ref().map(DirNode::total_size)
    }

    /// ルートからの相対パスのディレクトリ。ルートは空のパス。
    /// `..` などルートからの相対パスとして不正なパスは `None`。
    pub fn find_dir(&self, relative_path: &std::path::Path) -> Option<&DirNode> {
        let components = normal_components(relative_path)?;
        let mut current = self.root.as_ref()?;
        for component in components {
            current = current.child(component)?;
        }
        Some(current)
    }

    /// 部分スキャンの要求パスを名前で辿り、辿れた一番深いディレクトリを実際に走査する対象
    /// として返す（設計書 6.6 の表）。ファイルシステムには触れない。
    ///
    /// - 存在するディレクトリはそのまま。
    /// - 途中までしかないパス、ファイルを指すパス（ファイルはディレクトリとして載っていない）、
    ///   除外されて載っていないパス、シンボリックリンク（ディレクトリとして載らない）は、
    ///   辿れた一番深いディレクトリ。
    /// - 要求のパスが `..` などを含むなど不正なら、ルートまでの分だけ辿った結果になる
    ///   （検証は [`crate::dir_scan::request::PartialPath`] の責務）。
    ///
    /// 未スキャンなら `None`（その場合 [`crate::dir_scan::request::effective_target`] が
    /// フルスキャンに格上げする）。
    pub fn resolve_partial_target(
        &self,
        requested_path: &std::path::Path,
    ) -> Option<ResolvedTarget> {
        let mut current = self.root.as_ref()?;
        let mut effective_path = std::path::PathBuf::new();
        let mut exact = true;
        for component in requested_path.components() {
            let name = match component {
                std::path::Component::Normal(name) => name,
                std::path::Component::CurDir => continue,
                _ => {
                    exact = false;
                    break;
                }
            };
            match current.child(name) {
                Some(child) => {
                    current = child;
                    effective_path.push(name);
                }
                None => {
                    exact = false;
                    break;
                }
            }
        }
        Some(ResolvedTarget {
            effective_path,
            exact,
        })
    }

    /// フルスキャンの結果を反映する（設計書 6.6）。ルートのノードを入れ替え、除外パターン・
    /// ハッシュ生成器・状態を更新し、`generation` を 1 増やす。
    /// `scan_had_errors` はスキャンが「エラーありで成功」だったか。
    /// 外した旧ツリーは戻り値で返すので、ロックを手放してから解放する。
    /// `generation` が溢れる場合は何も変更せず [`TreeError::GenerationOverflow`]。
    pub fn apply_full(
        &mut self,
        scanned: FullScanTree,
        scan_had_errors: bool,
        finished_at: std::time::SystemTime,
    ) -> Result<ApplyFullOutcome, TreeError> {
        let generation_after = self
            .generation
            .checked_add(1)
            .ok_or(TreeError::GenerationOverflow)?;
        let generation_before = self.generation;
        let retired = RetiredFullTree {
            root: self.root.replace(scanned.root),
            name_hasher: std::mem::replace(&mut self.name_hasher, scanned.name_hasher),
            exclude_patterns: self.exclude_patterns.replace(scanned.exclude_patterns),
        };
        self.generation = generation_after;
        self.last_scan = Some(LastScan {
            outcome: success_outcome(scan_had_errors),
            finished_at,
        });
        Ok(ApplyFullOutcome {
            generation_before,
            generation_after,
            retired,
        })
    }

    /// 部分スキャンの結果で、`effective_path` の部分木を入れ替える（設計書 6.6）。
    ///
    /// `new_subtree` は [`DirNode::finalize`] 済みで、名前が `effective_path` の最後の要素と
    /// 一致していること（ルートを入れ替える `effective_path` が空のときは名前を問わない）。
    /// 経路上の祖先の `total_size` と `subtree_error_dir_count` は「古い部分木の値を引いて
    /// 新しい値を足す」。二段階で行い、先に祖先ごとの新しい値と `generation` をすべて
    /// 計算し（`checked_sub`／`checked_add`）、全部が成功してから書き込む。失敗したときは
    /// 何も変更しない。`checked_sub` の失敗（祖先の合計が子の合計より小さい）は
    /// [`TreeError::Inconsistent`] で、呼び出し側が `abort` する。
    /// `effective_path` が空なら、ルートのノード自体を入れ替える。除外パターンは変えない。
    /// `generation` はエラーの有無を問わず 1 増やす。
    pub fn apply_partial(
        &mut self,
        effective_path: &std::path::Path,
        new_subtree: DirNode,
        scan_had_errors: bool,
        finished_at: std::time::SystemTime,
    ) -> Result<ApplyPartialOutcome, TreeError> {
        let generation_before = self.generation;
        let generation_after = generation_before
            .checked_add(1)
            .ok_or(TreeError::GenerationOverflow)?;
        let components = normal_components(effective_path)
            .ok_or_else(|| TreeError::PathNotFound(effective_path.to_path_buf()))?;
        let root = self.root.as_mut().ok_or(TreeError::NotScanned)?;
        let retired_subtree = match components.split_last() {
            None => std::mem::replace(root, new_subtree),
            Some((target_name, ancestor_names)) => {
                if new_subtree.name() != *target_name {
                    return Err(TreeError::Inconsistent(format!(
                        "name of the new subtree ({}) does not match the target path ({})",
                        crate::dir_scan::report::escape_os_str_for_log(new_subtree.name()),
                        crate::dir_scan::report::escape_path_for_log(effective_path)
                    )));
                }
                // 第一段階：祖先ごとの新しい値をすべて計算する（書き込まない）
                let updates = plan_ancestor_updates(
                    root,
                    ancestor_names,
                    target_name,
                    &new_subtree,
                    effective_path,
                )?;
                // 第二段階：書き込む。第一段階で経路が存在することを確かめてある
                write_ancestor_updates(
                    root,
                    ancestor_names,
                    target_name,
                    new_subtree,
                    &updates,
                    effective_path,
                )?
            }
        };
        self.generation = generation_after;
        self.last_scan = Some(LastScan {
            outcome: success_outcome(scan_had_errors),
            finished_at,
        });
        Ok(ApplyPartialOutcome {
            generation_before,
            generation_after,
            retired_subtree,
        })
    }

    /// ツリーに反映されなかったスキャン（失敗）を記録する。直近のスキャンの結果だけを
    /// 「失敗」にし、ツリー・`generation`・ハッシュ生成器・除外パターンは変えない。
    /// フルスキャンでルートが開けなかった場合（前のツリーが残る。未スキャンなら未スキャンの
    /// まま）、部分スキャンの対象が `EACCES` などで開けなかった場合、走査中のパニックの
    /// 場合に使う。
    pub fn record_failed_scan(&mut self, reason: String, finished_at: std::time::SystemTime) {
        self.last_scan = Some(LastScan {
            outcome: crate::dir_scan::report::ScanOutcome::Failed(reason),
            finished_at,
        });
    }
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

fn success_outcome(scan_had_errors: bool) -> crate::dir_scan::report::ScanOutcome {
    if scan_had_errors {
        crate::dir_scan::report::ScanOutcome::SuccessWithErrors
    } else {
        crate::dir_scan::report::ScanOutcome::Success
    }
}

/// 相対パスの `Normal` な要素だけを取り出す（`.` は飛ばす）。`..`・絶対パスは `None`。
fn normal_components(path: &std::path::Path) -> Option<Vec<&std::ffi::OsStr>> {
    let mut names: Vec<&std::ffi::OsStr> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(name) => names.push(name),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(names)
}

/// 祖先 1 つ分の、更新後の値。
#[derive(Clone, Copy, Debug)]
struct AncestorUpdate {
    total_size: u128,
    subtree_error_dir_count: u64,
}

/// 第一段階。ルートから対象の親までの祖先（ルートを含む）それぞれの更新後の値を、
/// 古い部分木の値を引いて新しい部分木の値を足して計算する。何も書き込まない。
/// 戻り値の長さは `ancestor_names.len() + 1`（先頭がルート）。
fn plan_ancestor_updates(
    root: &DirNode,
    ancestor_names: &[&std::ffi::OsStr],
    target_name: &std::ffi::OsStr,
    new_subtree: &DirNode,
    effective_path: &std::path::Path,
) -> Result<Vec<AncestorUpdate>, TreeError> {
    let mut ancestors: Vec<&DirNode> = Vec::with_capacity(ancestor_names.len() + 1);
    ancestors.push(root);
    for name in ancestor_names {
        let parent = ancestors[ancestors.len() - 1];
        let next = parent
            .child(name)
            .ok_or_else(|| TreeError::PathNotFound(effective_path.to_path_buf()))?;
        ancestors.push(next);
    }
    let target_parent = ancestors[ancestors.len() - 1];
    let old_subtree = target_parent
        .child(target_name)
        .ok_or_else(|| TreeError::PathNotFound(effective_path.to_path_buf()))?;
    let mut updates: Vec<AncestorUpdate> = Vec::with_capacity(ancestors.len());
    for ancestor in ancestors {
        let total_size = ancestor
            .total_size
            .checked_sub(old_subtree.total_size)
            .ok_or_else(|| {
                TreeError::Inconsistent(format!(
                    "total_size of an ancestor ({}) is smaller than that of the old subtree ({}) at {}",
                    ancestor.total_size,
                    old_subtree.total_size,
                    crate::dir_scan::report::escape_path_for_log(effective_path)
                ))
            })?
            .checked_add(new_subtree.total_size)
            .ok_or_else(|| TreeError::Inconsistent("total_size of an ancestor overflowed".to_string()))?;
        let subtree_error_dir_count = ancestor
            .subtree_error_dir_count
            .checked_sub(old_subtree.subtree_error_dir_count)
            .ok_or_else(|| {
                TreeError::Inconsistent(format!(
                    "subtree_error_dir_count of an ancestor ({}) is smaller than that of the old subtree ({}) at {}",
                    ancestor.subtree_error_dir_count,
                    old_subtree.subtree_error_dir_count,
                    crate::dir_scan::report::escape_path_for_log(effective_path)
                ))
            })?
            .checked_add(new_subtree.subtree_error_dir_count)
            .ok_or_else(|| {
                TreeError::Inconsistent(
                    "subtree_error_dir_count of an ancestor overflowed".to_string(),
                )
            })?;
        updates.push(AncestorUpdate {
            total_size,
            subtree_error_dir_count,
        });
    }
    Ok(updates)
}

/// 第二段階。計算済みの値を祖先に書き込み、対象の部分木を入れ替えて旧部分木を返す。
/// メモリの確保は行わない（既存のエントリの入れ替えだけ）。
fn write_ancestor_updates(
    root: &mut DirNode,
    ancestor_names: &[&std::ffi::OsStr],
    target_name: &std::ffi::OsStr,
    new_subtree: DirNode,
    updates: &[AncestorUpdate],
    effective_path: &std::path::Path,
) -> Result<DirNode, TreeError> {
    if updates.len() != ancestor_names.len() + 1 {
        return Err(TreeError::Inconsistent(
            "number of planned updates does not match the path".to_string(),
        ));
    }
    let not_found = || TreeError::PathNotFound(effective_path.to_path_buf());
    let mut current: &mut DirNode = root;
    current.total_size = updates[0].total_size;
    current.subtree_error_dir_count = updates[0].subtree_error_dir_count;
    for (name, update) in ancestor_names.iter().zip(&updates[1..]) {
        current = current.children.get_mut(*name).ok_or_else(not_found)?;
        current.total_size = update.total_size;
        current.subtree_error_dir_count = update.subtree_error_dir_count;
    }
    let slot = current
        .children
        .get_mut(target_name)
        .ok_or_else(not_found)?;
    Ok(std::mem::replace(slot, new_subtree))
}

/// ディレクトリ 1 つ分の読み取り結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryRead {
    /// 直下のファイルの合計
    pub own_size: u128,
    /// 配下すべての合計
    pub total_size: u128,
    /// `total_size` が完全か（配下にエラーを持つディレクトリがないか）
    pub complete: bool,
    /// 子ディレクトリごとの合計（名前順）
    pub children: Vec<ChildRead>,
}

/// 子ディレクトリ 1 つ分の読み取り結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildRead {
    pub name: std::ffi::OsString,
    pub total_size: u128,
    /// `total_size` が完全か
    pub complete: bool,
}

/// 指定したディレクトリの検索結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryLookup {
    Found(DirectoryRead),
    /// 未スキャン、またはツリーにそのディレクトリがない（存在しない・除外されている・
    /// ディレクトリではない・パスが不正）
    NotFound,
}

/// ツリーの読み取りの結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRead {
    pub status: TreeStatus,
    pub directory: DirectoryLookup,
}

/// ツリーの状態と、指定したディレクトリ（ルートからの相対パス、ルートは空のパス）の
/// 合計・子ディレクトリごとの合計を、それぞれ「完全か」と一緒に読む（設計書 6.2）。
/// 読み取りロックの中で呼ぶ。
pub fn read_directory(tree: &Tree, relative_path: &std::path::Path) -> TreeRead {
    let directory = match tree.find_dir(relative_path) {
        None => DirectoryLookup::NotFound,
        Some(node) => {
            let mut children: Vec<ChildRead> = node
                .children()
                .values()
                .map(|child| ChildRead {
                    name: child.name().to_os_string(),
                    total_size: child.total_size(),
                    complete: child.is_complete(),
                })
                .collect();
            children.sort_by(|a, b| a.name.cmp(&b.name));
            DirectoryLookup::Found(DirectoryRead {
                own_size: node.own_size(),
                total_size: node.total_size(),
                complete: node.is_complete(),
                children,
            })
        }
    };
    TreeRead {
        status: tree.status(),
        directory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_logger() {
        env_logger::builder().is_test(true).try_init().ok();
    }

    fn os(name: &str) -> std::ffi::OsString {
        std::ffi::OsString::from(name)
    }

    fn file(name: &str, size: u64) -> (std::ffi::OsString, u64) {
        (os(name), size)
    }

    /// 名前の長さをハッシュにする。同じ長さの名前は必ず衝突する。
    fn collide_by_length(bytes: &[u8]) -> u64 {
        bytes.len() as u64
    }

    fn colliding_hasher() -> NameHasher {
        NameHasher::with_test_mapping(collide_by_length)
    }

    fn dir_with_files(name: &str, hasher: &NameHasher, files: &[(&str, u64)]) -> DirNode {
        let mut node = DirNode::new(os(name), 1, 1);
        let owned: Vec<(std::ffi::OsString, u64)> =
            files.iter().map(|(n, s)| file(n, *s)).collect();
        node.set_files(hasher, &owned).unwrap();
        node
    }

    #[test]
    fn name_hasher_is_stable_for_the_same_name_and_clone() {
        init_logger();
        let hasher = NameHasher::new();
        let cloned = hasher.clone();
        let name = os("file.txt");
        assert_eq!(hasher.hash_name(&name), hasher.hash_name(&name));
        assert_eq!(hasher.hash_name(&name), cloned.hash_name(&name));
        assert_ne!(hasher.hash_name(&name), hasher.hash_name(&os("file.txr")));
    }

    #[test]
    fn name_hasher_hashes_non_utf8_bytes() {
        init_logger();
        let hasher = NameHasher::new();
        let non_utf8 = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"a\xffb");
        let other = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"a\xfeb");
        assert_eq!(hasher.hash_name(non_utf8), hasher.hash_name(non_utf8));
        assert_ne!(hasher.hash_name(non_utf8), hasher.hash_name(other));
    }

    #[test]
    fn name_hasher_test_mapping_forces_collisions() {
        init_logger();
        let hasher = colliding_hasher();
        assert_eq!(hasher.hash_name(&os("ab")), hasher.hash_name(&os("cd")));
        assert_ne!(hasher.hash_name(&os("ab")), hasher.hash_name(&os("abc")));
    }

    #[test]
    fn file_entry_stays_small() {
        init_logger();
        // 単独のときはサイズだけで、衝突したときだけ一覧を持つ（ハッシュ表の要素は
        // ハッシュ 8 バイトとエントリ）。
        assert_eq!(std::mem::size_of::<FileEntry>(), 16);
        assert_eq!(std::mem::size_of::<u64>(), 8);
    }

    #[test]
    fn set_files_without_collision_keeps_single_entries() {
        init_logger();
        let hasher = NameHasher::new();
        let mut node = DirNode::new(os("d"), 1, 2);
        let collisions = node
            .set_files(&hasher, &[file("a", 10), file("bb", 20), file("ccc", 30)])
            .unwrap();
        assert_eq!(collisions, 0);
        assert_eq!(node.files().len(), 3);
        assert!(node
            .files()
            .values()
            .all(|entry| matches!(entry, FileEntry::Single(_))));
        node.finalize().unwrap();
        assert_eq!(node.own_size(), 60);
        assert_eq!(node.total_size(), 60);
        let counts = node.counts();
        assert_eq!(
            counts,
            TreeCounts {
                dir_count: 1,
                file_count: 3,
                hash_collision_count: 0
            }
        );
    }

    #[test]
    fn set_files_with_collisions_keeps_real_names_and_sizes() {
        init_logger();
        let hasher = colliding_hasher();
        let mut node = DirNode::new(os("d"), 1, 2);
        // "a","b","c" は長さ 1 で衝突、"dd","ee" は長さ 2 で衝突、"fff" は単独
        let collisions = node
            .set_files(
                &hasher,
                &[
                    file("a", 1),
                    file("dd", 40),
                    file("b", 2),
                    file("fff", 500),
                    file("c", 3),
                    file("ee", 50),
                ],
            )
            .unwrap();
        assert_eq!(collisions, 3);
        assert_eq!(node.files().len(), 3);
        node.finalize().unwrap();
        assert_eq!(node.own_size(), 1 + 2 + 3 + 40 + 50 + 500);
        assert_eq!(node.total_size(), node.own_size());
        let counts = node.counts();
        assert_eq!(counts.file_count, 6);
        assert_eq!(counts.hash_collision_count, 3);
        match node.files().get(&1).unwrap() {
            FileEntry::Collided(files) => {
                let mut names: Vec<&std::ffi::OsString> = files.iter().map(|f| &f.name).collect();
                names.sort();
                assert_eq!(names, vec![&os("a"), &os("b"), &os("c")]);
            }
            FileEntry::Single(_) => panic!("expected a collided entry"),
        }
    }

    #[test]
    fn set_files_rejects_duplicate_names_without_changing_the_node() {
        init_logger();
        let hasher = NameHasher::new();
        let mut node = DirNode::new(os("d"), 1, 2);
        let result = node.set_files(&hasher, &[file("a", 1), file("b", 2), file("a", 3)]);
        assert_eq!(result, Err(TreeError::DuplicateName(os("a"))));
        assert!(node.files().is_empty());
    }

    #[test]
    fn set_files_twice_is_inconsistent() {
        init_logger();
        let hasher = NameHasher::new();
        let mut node = DirNode::new(os("d"), 1, 2);
        node.set_files(&hasher, &[file("a", 1)]).unwrap();
        let result = node.set_files(&hasher, &[file("b", 2)]);
        assert!(matches!(result, Err(TreeError::Inconsistent(_))));
        assert!(result.unwrap_err().is_fatal());
        assert_eq!(node.files().len(), 1);
    }

    #[test]
    fn finalize_propagates_sizes_from_children() {
        init_logger();
        let hasher = NameHasher::new();
        let mut leaf = dir_with_files("leaf", &hasher, &[("x", 5), ("y", 7)]);
        leaf.finalize().unwrap();
        let mut mid = dir_with_files("mid", &hasher, &[("z", 100)]);
        mid.add_child(leaf).unwrap();
        mid.finalize().unwrap();
        let mut root = dir_with_files("", &hasher, &[("r", 1000)]);
        root.add_child(mid).unwrap();
        root.finalize().unwrap();
        assert_eq!(root.own_size(), 1000);
        assert_eq!(root.total_size(), 1112);
        let mid = root.child(&os("mid")).unwrap();
        assert_eq!(mid.own_size(), 100);
        assert_eq!(mid.total_size(), 112);
        assert_eq!(mid.child(&os("leaf")).unwrap().total_size(), 12);
        assert_eq!(root.counts().dir_count, 3);
        assert_eq!(root.counts().file_count, 4);
    }

    #[test]
    fn finalize_counts_error_directories() {
        init_logger();
        let hasher = NameHasher::new();
        let mut bad = dir_with_files("bad", &hasher, &[]);
        bad.record_open_error(libc::EACCES);
        bad.finalize().unwrap();
        assert_eq!(bad.subtree_error_dir_count(), 1);
        assert!(!bad.is_complete());
        let mut good = dir_with_files("good", &hasher, &[("a", 1)]);
        good.finalize().unwrap();
        assert!(good.is_complete());
        let mut root = dir_with_files("", &hasher, &[]);
        root.record_stat_error(libc::EIO);
        root.add_child(bad).unwrap();
        root.add_child(good).unwrap();
        root.finalize().unwrap();
        // root 自身と bad
        assert_eq!(root.subtree_error_dir_count(), 2);
        assert_eq!(
            root.child(&os("good")).unwrap().subtree_error_dir_count(),
            0
        );
        assert_eq!(root.errors().stat_eio, 1);
    }

    #[test]
    fn record_stat_error_classifies_errno_and_ignores_enoent() {
        init_logger();
        let mut node = DirNode::new(os("d"), 1, 1);
        node.record_stat_error(libc::ENOENT);
        assert!(!node.errors().has_error());
        node.record_stat_error(libc::EACCES);
        node.record_stat_error(libc::EACCES);
        node.record_stat_error(libc::EIO);
        node.record_stat_error(libc::EINVAL);
        assert_eq!(node.errors().stat_eacces, 2);
        assert_eq!(node.errors().stat_eio, 1);
        assert_eq!(node.errors().stat_other, 1);
        let mut cyclic = DirNode::new(os("c"), 1, 1);
        cyclic.record_cycle();
        cyclic.record_read_dir_error(libc::EIO);
        assert!(cyclic.errors().cycle_detected);
        assert_eq!(cyclic.errors().read_dir_errno, Some(libc::EIO));
    }

    #[test]
    fn finalize_detects_overflow_without_changing_the_node() {
        init_logger();
        let hasher = NameHasher::new();
        let mut child = dir_with_files("c", &hasher, &[]);
        child.total_size = u128::MAX;
        let mut parent = dir_with_files("p", &hasher, &[("a", 1)]);
        parent.add_child(child).unwrap();
        let result = parent.finalize();
        assert!(matches!(result, Err(TreeError::Inconsistent(_))));
        assert_eq!(parent.total_size(), 0);
        assert_eq!(parent.own_size(), 0);
    }

    #[test]
    fn add_child_rejects_duplicate_names() {
        init_logger();
        let mut parent = DirNode::new(os("p"), 1, 1);
        parent.add_child(DirNode::new(os("c"), 1, 2)).unwrap();
        let result = parent.add_child(DirNode::new(os("c"), 1, 3));
        assert_eq!(result, Err(TreeError::DuplicateName(os("c"))));
        assert_eq!(parent.children().len(), 1);
    }

    // ---- Tree（状態・反映・読み取り）のテスト ----

    const T0: std::time::SystemTime = std::time::UNIX_EPOCH;

    /// 完成したディレクトリを作る。`broken` なら stat のエラーを付ける。
    fn dir(
        hasher: &NameHasher,
        name: &str,
        files: &[(&str, u64)],
        broken: bool,
        children: Vec<DirNode>,
    ) -> DirNode {
        let mut node = dir_with_files(name, hasher, files);
        if broken {
            node.record_stat_error(libc::EACCES);
        }
        for child in children {
            node.add_child(child).unwrap();
        }
        node.finalize().unwrap();
        node
    }

    fn patterns(list: &[&str]) -> crate::dir_scan::settings::ExcludePatterns {
        let owned: Vec<String> = list.iter().map(|p| p.to_string()).collect();
        crate::dir_scan::settings::ExcludePatterns::new(&owned).unwrap()
    }

    /// root(r=1){ a(a1=10){ b(100,200), c() }, z(5) }
    fn sample_root(hasher: &NameHasher) -> DirNode {
        let b = dir(hasher, "b", &[("b1", 100), ("b2", 200)], false, vec![]);
        let c = dir(hasher, "c", &[], false, vec![]);
        let a = dir(hasher, "a", &[("a1", 10)], false, vec![b, c]);
        let z = dir(hasher, "z", &[("z1", 5)], false, vec![]);
        dir(hasher, "", &[("r", 1)], false, vec![a, z])
    }

    fn full_tree(root: DirNode, hasher: NameHasher, pattern_list: &[&str]) -> FullScanTree {
        FullScanTree {
            root,
            exclude_patterns: patterns(pattern_list),
            name_hasher: hasher,
        }
    }

    fn scanned_tree(hasher: NameHasher) -> Tree {
        let mut tree = Tree::new();
        let root = sample_root(&hasher);
        tree.apply_full(full_tree(root, hasher, &["**/.*"]), false, T0)
            .unwrap();
        tree
    }

    fn total_of(tree: &Tree, path: &str) -> u128 {
        tree.find_dir(std::path::Path::new(path))
            .unwrap()
            .total_size()
    }

    #[test]
    fn new_tree_is_unscanned() {
        init_logger();
        let tree = Tree::new();
        assert_eq!(tree.generation(), 0);
        assert_eq!(tree.snapshot_state(), SnapshotState::Unscanned);
        assert!(tree.last_scan().is_none());
        assert!(tree.exclude_patterns().is_none());
        assert!(tree.root().is_none());
        assert!(tree
            .resolve_partial_target(std::path::Path::new("a"))
            .is_none());
        let read = read_directory(&tree, std::path::Path::new(""));
        assert_eq!(read.directory, DirectoryLookup::NotFound);
        assert_eq!(read.status.snapshot, SnapshotState::Unscanned);
    }

    #[test]
    fn partial_apply_on_unscanned_tree_is_rejected() {
        init_logger();
        let hasher = NameHasher::new();
        let mut tree = Tree::new();
        let result = tree.apply_partial(
            std::path::Path::new(""),
            dir(&hasher, "", &[], false, vec![]),
            false,
            T0,
        );
        assert_eq!(result.err(), Some(TreeError::NotScanned));
        assert_eq!(tree.generation(), 0);
        assert!(tree.last_scan().is_none());
    }

    #[test]
    fn full_apply_moves_unscanned_to_complete() {
        init_logger();
        let hasher = NameHasher::new();
        let tree = scanned_tree(hasher);
        assert_eq!(tree.generation(), 1);
        assert_eq!(tree.snapshot_state(), SnapshotState::Complete);
        assert_eq!(tree.exclude_patterns(), Some(&patterns(&["**/.*"])));
        assert_eq!(
            tree.last_scan(),
            Some(&LastScan {
                outcome: crate::dir_scan::report::ScanOutcome::Success,
                finished_at: T0
            })
        );
        assert_eq!(tree.root_total_size(), Some(316));
    }

    #[test]
    fn full_apply_with_errors_is_incomplete_and_returns_the_old_tree() {
        init_logger();
        let hasher = NameHasher::new();
        let mut tree = scanned_tree(hasher.clone());
        let new_hasher = NameHasher::new();
        let broken = dir(&new_hasher, "bad", &[], true, vec![]);
        let root = dir(&new_hasher, "", &[("x", 7)], false, vec![broken]);
        let new_hash_of_x = new_hasher.hash_name(&os("x"));
        let outcome = tree
            .apply_full(full_tree(root, new_hasher, &["tmp"]), true, T0)
            .unwrap();
        assert_eq!(outcome.generation_before, 1);
        assert_eq!(outcome.generation_after, 2);
        assert_eq!(tree.generation(), 2);
        assert_eq!(tree.snapshot_state(), SnapshotState::Incomplete);
        assert_eq!(
            tree.last_scan().unwrap().outcome,
            crate::dir_scan::report::ScanOutcome::SuccessWithErrors
        );
        // 除外パターンとハッシュ生成器が新しいものに替わり、旧ツリーが戻り値で返る
        assert_eq!(tree.exclude_patterns(), Some(&patterns(&["tmp"])));
        assert_eq!(tree.name_hasher().hash_name(&os("x")), new_hash_of_x);
        assert_eq!(outcome.retired.exclude_patterns, Some(patterns(&["**/.*"])));
        assert_eq!(outcome.retired.root.unwrap().total_size(), 316);
        assert_eq!(
            outcome.retired.name_hasher.hash_name(&os("x")),
            hasher.hash_name(&os("x"))
        );
        // エラーのないフルスキャンで完全に戻る
        let clean = sample_root(tree.name_hasher());
        let hasher = tree.name_hasher().clone();
        tree.apply_full(full_tree(clean, hasher, &["tmp"]), false, T0)
            .unwrap();
        assert_eq!(tree.snapshot_state(), SnapshotState::Complete);
        assert_eq!(tree.generation(), 3);
    }

    #[test]
    fn failed_scan_keeps_the_previous_tree_and_generation() {
        init_logger();
        let mut tree = scanned_tree(NameHasher::new());
        let later = T0 + std::time::Duration::from_secs(5);
        tree.record_failed_scan("cannot open root".to_string(), later);
        assert_eq!(tree.generation(), 1);
        assert_eq!(tree.snapshot_state(), SnapshotState::Complete);
        assert_eq!(tree.root_total_size(), Some(316));
        assert_eq!(tree.exclude_patterns(), Some(&patterns(&["**/.*"])));
        assert_eq!(
            tree.last_scan(),
            Some(&LastScan {
                outcome: crate::dir_scan::report::ScanOutcome::Failed(
                    "cannot open root".to_string()
                ),
                finished_at: later
            })
        );
    }

    #[test]
    fn failed_scan_on_unscanned_tree_stays_unscanned() {
        init_logger();
        let mut tree = Tree::new();
        tree.record_failed_scan("cannot open root".to_string(), T0);
        assert_eq!(tree.generation(), 0);
        assert_eq!(tree.snapshot_state(), SnapshotState::Unscanned);
        assert!(tree.exclude_patterns().is_none());
        assert!(matches!(
            tree.last_scan().unwrap().outcome,
            crate::dir_scan::report::ScanOutcome::Failed(_)
        ));
    }

    #[test]
    fn generation_overflow_changes_nothing() {
        init_logger();
        let hasher = NameHasher::new();
        let mut tree = scanned_tree(hasher.clone());
        tree.generation = u64::MAX;
        let root = dir(&hasher, "", &[("x", 1)], false, vec![]);
        let result = tree.apply_full(full_tree(root, hasher.clone(), &[]), false, T0);
        assert_eq!(result.err(), Some(TreeError::GenerationOverflow));
        assert!(TreeError::GenerationOverflow.is_fatal());
        assert_eq!(tree.root_total_size(), Some(316));
        assert_eq!(tree.exclude_patterns(), Some(&patterns(&["**/.*"])));
        let b = dir(&hasher, "b", &[], false, vec![]);
        let result = tree.apply_partial(std::path::Path::new("a/b"), b, false, T0);
        assert_eq!(result.err(), Some(TreeError::GenerationOverflow));
        assert_eq!(total_of(&tree, "a/b"), 300);
        assert_eq!(tree.generation(), u64::MAX);
    }

    #[test]
    fn resolve_partial_target_covers_the_design_table() {
        init_logger();
        // a/b がディレクトリ。.cache は除外、link はリンクなので載っていない。
        let tree = scanned_tree(NameHasher::new());
        let resolve = |path: &str| {
            tree.resolve_partial_target(std::path::Path::new(path))
                .unwrap()
        };
        let exact = |path: &str| ResolvedTarget {
            effective_path: std::path::PathBuf::from(path),
            exact: true,
        };
        let partial = |path: &str| ResolvedTarget {
            effective_path: std::path::PathBuf::from(path),
            exact: false,
        };
        // 存在するディレクトリ
        assert_eq!(resolve("a/b"), exact("a/b"));
        // 途中まで
        assert_eq!(resolve("a/new/x"), partial("a"));
        assert_eq!(resolve("new/x/y"), partial(""));
        // ファイルを指す（ファイルはディレクトリとして載っていない）
        assert_eq!(resolve("a/b/file.txt"), partial("a/b"));
        assert_eq!(resolve("a/a1"), partial("a"));
        // 除外対象、シンボリックリンク（どちらも載っていない）
        assert_eq!(resolve("a/.cache/x"), partial("a"));
        assert_eq!(resolve("a/link/x"), partial("a"));
        // ルート自身（空のパス）
        assert_eq!(resolve(""), exact(""));
        // 不正なパスはそこまでで止まる
        assert_eq!(resolve("a/../z"), partial("a"));
    }

    #[test]
    fn resolve_partial_target_handles_non_utf8_names() {
        init_logger();
        let hasher = NameHasher::new();
        let weird = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"d\xff");
        let child = dir(&hasher, "x", &[("f", 1)], false, vec![]);
        let mut named = DirNode::new(weird.to_os_string(), 1, 1);
        named.add_child(child).unwrap();
        named.finalize().unwrap();
        let mut root = DirNode::new(os(""), 1, 1);
        root.add_child(named).unwrap();
        root.finalize().unwrap();
        let mut tree = Tree::new();
        tree.apply_full(full_tree(root, hasher, &[]), false, T0)
            .unwrap();
        let mut requested = std::path::PathBuf::from(weird);
        requested.push("x");
        requested.push("missing");
        let resolved = tree.resolve_partial_target(&requested).unwrap();
        assert_eq!(
            resolved.effective_path,
            std::path::PathBuf::from(weird).join("x")
        );
        assert!(!resolved.exact);
    }

    #[test]
    fn partial_apply_replaces_subtree_and_updates_ancestors() {
        init_logger();
        let hasher = NameHasher::new();
        let mut tree = scanned_tree(hasher.clone());
        let new_b = dir(&hasher, "b", &[("n", 1000)], false, vec![]);
        let outcome = tree
            .apply_partial(std::path::Path::new("a/b"), new_b, false, T0)
            .unwrap();
        assert_eq!(outcome.generation_before, 1);
        assert_eq!(outcome.generation_after, 2);
        assert_eq!(outcome.retired_subtree.total_size(), 300);
        assert_eq!(tree.generation(), 2);
        assert_eq!(total_of(&tree, "a/b"), 1000);
        assert_eq!(total_of(&tree, "a"), 1010);
        assert_eq!(total_of(&tree, ""), 1016);
        // 兄弟は変わらない
        assert_eq!(total_of(&tree, "z"), 5);
        // 除外パターンは変わらない
        assert_eq!(tree.exclude_patterns(), Some(&patterns(&["**/.*"])));
        assert_eq!(tree.snapshot_state(), SnapshotState::Complete);
    }

    #[test]
    fn partial_apply_with_empty_result_shrinks_ancestors() {
        init_logger();
        let hasher = NameHasher::new();
        let mut tree = scanned_tree(hasher.clone());
        let empty_b = dir(&hasher, "b", &[], false, vec![]);
        tree.apply_partial(std::path::Path::new("a/b"), empty_b, false, T0)
            .unwrap();
        assert_eq!(total_of(&tree, "a"), 10);
        assert_eq!(total_of(&tree, ""), 16);
    }

    #[test]
    fn partial_apply_propagates_error_directory_counts() {
        init_logger();
        let hasher = NameHasher::new();
        let mut tree = scanned_tree(hasher.clone());
        let broken_c = dir(
            &hasher,
            "c",
            &[],
            true,
            vec![dir(&hasher, "d", &[], true, vec![])],
        );
        tree.apply_partial(std::path::Path::new("a/c"), broken_c, true, T0)
            .unwrap();
        assert_eq!(tree.snapshot_state(), SnapshotState::Incomplete);
        assert_eq!(
            tree.last_scan().unwrap().outcome,
            crate::dir_scan::report::ScanOutcome::SuccessWithErrors
        );
        let read = read_directory(&tree, std::path::Path::new("a"));
        match read.directory {
            DirectoryLookup::Found(found) => {
                assert!(!found.complete);
                let by_name: Vec<(&str, bool)> = found
                    .children
                    .iter()
                    .map(|c| (c.name.to_str().unwrap(), c.complete))
                    .collect();
                assert_eq!(by_name, vec![("b", true), ("c", false)]);
            }
            DirectoryLookup::NotFound => panic!("a should be found"),
        }
        assert_eq!(
            tree.find_dir(std::path::Path::new(""))
                .unwrap()
                .subtree_error_dir_count(),
            2
        );
        // z は影響を受けない
        assert!(tree
            .find_dir(std::path::Path::new("z"))
            .unwrap()
            .is_complete());
        // エラーのない部分木に差し替えると完全に戻る
        let clean_c = dir(&hasher, "c", &[("k", 3)], false, vec![]);
        tree.apply_partial(std::path::Path::new("a/c"), clean_c, false, T0)
            .unwrap();
        assert_eq!(tree.snapshot_state(), SnapshotState::Complete);
        assert_eq!(tree.generation(), 3);
        assert_eq!(total_of(&tree, ""), 319);
    }

    #[test]
    fn partial_apply_at_root_replaces_the_root_and_keeps_patterns() {
        init_logger();
        let hasher = NameHasher::new();
        let mut tree = scanned_tree(hasher.clone());
        let new_root = dir(&hasher, "", &[("only", 9)], false, vec![]);
        let outcome = tree
            .apply_partial(std::path::Path::new(""), new_root, false, T0)
            .unwrap();
        assert_eq!(outcome.retired_subtree.total_size(), 316);
        assert_eq!(tree.root_total_size(), Some(9));
        assert_eq!(tree.exclude_patterns(), Some(&patterns(&["**/.*"])));
        assert_eq!(tree.generation(), 2);
    }

    #[test]
    fn partial_apply_rejects_bad_arguments_without_changes() {
        init_logger();
        let hasher = NameHasher::new();
        let mut tree = scanned_tree(hasher.clone());
        let new_node = |name: &str| dir(&hasher, name, &[("n", 1)], false, vec![]);
        let not_found =
            tree.apply_partial(std::path::Path::new("a/zzz"), new_node("zzz"), false, T0);
        assert_eq!(
            not_found.err(),
            Some(TreeError::PathNotFound(std::path::PathBuf::from("a/zzz")))
        );
        let missing_parent =
            tree.apply_partial(std::path::Path::new("q/zzz"), new_node("zzz"), false, T0);
        assert!(matches!(
            missing_parent.err(),
            Some(TreeError::PathNotFound(_))
        ));
        let parent_dir =
            tree.apply_partial(std::path::Path::new("a/../z"), new_node("z"), false, T0);
        assert!(matches!(parent_dir.err(), Some(TreeError::PathNotFound(_))));
        let mismatch = tree.apply_partial(std::path::Path::new("a/b"), new_node("c"), false, T0);
        assert!(matches!(mismatch.err(), Some(TreeError::Inconsistent(_))));
        assert_eq!(tree.generation(), 1);
        assert_eq!(tree.root_total_size(), Some(316));
        assert_eq!(total_of(&tree, "a/b"), 300);
    }

    /// 木全体の値を取り出して、変更されていないことを比べる。
    fn snapshot_of(tree: &Tree) -> (u64, u128, u128, u128, u64, u64) {
        let root = tree.root().unwrap();
        let a = root.child(&os("a")).unwrap();
        let b = a.child(&os("b")).unwrap();
        (
            tree.generation(),
            root.total_size(),
            a.total_size(),
            b.total_size(),
            root.subtree_error_dir_count(),
            a.subtree_error_dir_count(),
        )
    }

    #[test]
    fn checked_sub_failure_is_detected_before_any_write() {
        init_logger();
        let hasher = NameHasher::new();
        // 途中の祖先 a の合計が、子 b の合計より小さい（内部不整合）
        let mut tree = scanned_tree(hasher.clone());
        tree.root
            .as_mut()
            .unwrap()
            .children
            .get_mut(&os("a"))
            .unwrap()
            .total_size = 50;
        let before = snapshot_of(&tree);
        let new_b = dir(&hasher, "b", &[("n", 1)], false, vec![]);
        let err = tree
            .apply_partial(std::path::Path::new("a/b"), new_b, false, T0)
            .err()
            .unwrap();
        assert!(matches!(err, TreeError::Inconsistent(_)));
        assert!(err.is_fatal());
        // ルートは先に更新される位置だが、二段階なので何も書かれていない
        assert_eq!(snapshot_of(&tree), before);
        assert_eq!(tree.last_scan().unwrap().finished_at, T0);

        // ルートの合計がおかしい場合
        let mut tree = scanned_tree(hasher.clone());
        tree.root.as_mut().unwrap().total_size = 0;
        let before = snapshot_of(&tree);
        let new_b = dir(&hasher, "b", &[("n", 1)], false, vec![]);
        let result = tree.apply_partial(std::path::Path::new("a/b"), new_b, false, T0);
        assert!(result.err().unwrap().is_fatal());
        assert_eq!(snapshot_of(&tree), before);
    }

    #[test]
    fn error_count_underflow_is_detected_before_any_write() {
        init_logger();
        let hasher = NameHasher::new();
        let broken_b = dir(&hasher, "b", &[("b1", 1)], true, vec![]);
        let a = dir(&hasher, "a", &[], false, vec![broken_b]);
        let root = dir(&hasher, "", &[], false, vec![a]);
        let mut tree = Tree::new();
        tree.apply_full(full_tree(root, hasher.clone(), &[]), true, T0)
            .unwrap();
        // a のエラーディレクトリ数が b より小さい
        tree.root
            .as_mut()
            .unwrap()
            .children
            .get_mut(&os("a"))
            .unwrap()
            .subtree_error_dir_count = 0;
        let before = snapshot_of(&tree);
        let new_b = dir(&hasher, "b", &[("n", 1)], false, vec![]);
        let result = tree.apply_partial(std::path::Path::new("a/b"), new_b, false, T0);
        assert!(result.err().unwrap().is_fatal());
        assert_eq!(snapshot_of(&tree), before);
    }

    #[test]
    fn total_size_overflow_on_partial_apply_is_inconsistent() {
        init_logger();
        let hasher = NameHasher::new();
        let mut tree = scanned_tree(hasher.clone());
        tree.root.as_mut().unwrap().total_size = u128::MAX;
        let before = snapshot_of(&tree);
        let mut huge = dir(&hasher, "b", &[("n", 1)], false, vec![]);
        huge.total_size = u128::MAX;
        let result = tree.apply_partial(std::path::Path::new("a/b"), huge, false, T0);
        assert!(result.err().unwrap().is_fatal());
        assert_eq!(snapshot_of(&tree), before);
    }

    #[test]
    fn read_directory_returns_totals_with_completeness() {
        init_logger();
        let hasher = NameHasher::new();
        let mut tree = scanned_tree(hasher.clone());
        let read = read_directory(&tree, std::path::Path::new(""));
        assert_eq!(read.status.generation, 1);
        assert_eq!(read.status.snapshot, SnapshotState::Complete);
        assert_eq!(
            read.directory,
            DirectoryLookup::Found(DirectoryRead {
                own_size: 1,
                total_size: 316,
                complete: true,
                children: vec![
                    ChildRead {
                        name: os("a"),
                        total_size: 310,
                        complete: true
                    },
                    ChildRead {
                        name: os("z"),
                        total_size: 5,
                        complete: true
                    },
                ],
            })
        );
        let missing = read_directory(&tree, std::path::Path::new("a/nothing"));
        assert_eq!(missing.directory, DirectoryLookup::NotFound);
        let invalid = read_directory(&tree, std::path::Path::new("../a"));
        assert_eq!(invalid.directory, DirectoryLookup::NotFound);
        let broken_z = dir(&hasher, "z", &[("z1", 5)], true, vec![]);
        tree.apply_partial(std::path::Path::new("z"), broken_z, true, T0)
            .unwrap();
        let read = read_directory(&tree, std::path::Path::new(""));
        match read.directory {
            DirectoryLookup::Found(found) => {
                assert!(!found.complete);
                assert!(found.children[0].complete);
                assert!(!found.children[1].complete);
            }
            DirectoryLookup::NotFound => panic!("root should be found"),
        }
        assert_eq!(read.status.snapshot, SnapshotState::Incomplete);
    }

    // ---- ハッシュ衝突中のファイルの削除・サイズ変更（実名で引く） ----

    /// テスト用。`dir_path` のディレクトリで、実名 `name` のファイルのサイズを `new_size` に
    /// 変える（`None` なら削除）。`own_size` を直し、祖先の `total_size` に差分を伝える。
    /// 将来の inotify によるファイル単位の更新の最小の形。
    fn change_file(tree: &mut Tree, dir_path: &str, name: &str, new_size: Option<u64>) {
        fn descend(
            node: &mut DirNode,
            components: &[&std::ffi::OsStr],
            hasher: &NameHasher,
            name: &std::ffi::OsStr,
            new_size: Option<u64>,
        ) -> (u128, u128) {
            let (old, new) = match components.split_first() {
                Some((first, rest)) => {
                    let child = node.children.get_mut(*first).unwrap();
                    descend(child, rest, hasher, name, new_size)
                }
                None => {
                    let hash = hasher.hash_name(name);
                    let entry = node.files.get_mut(&hash).unwrap();
                    let (old, remove_entry) = match entry {
                        FileEntry::Single(size) => {
                            let old = *size;
                            match new_size {
                                Some(new_size) => {
                                    *size = new_size;
                                    (old, false)
                                }
                                None => (old, true),
                            }
                        }
                        FileEntry::Collided(files) => {
                            let position = files.iter().position(|f| f.name == name).unwrap();
                            let old = files[position].size;
                            match new_size {
                                Some(new_size) => files[position].size = new_size,
                                None => {
                                    let mut remaining = std::mem::take(files).into_vec();
                                    remaining.remove(position);
                                    *files = remaining.into_boxed_slice();
                                }
                            }
                            (old, files.is_empty())
                        }
                    };
                    if remove_entry {
                        node.files.remove(&hash);
                    }
                    let new = new_size.unwrap_or(0);
                    node.own_size = node
                        .own_size
                        .checked_sub(u128::from(old))
                        .unwrap()
                        .checked_add(u128::from(new))
                        .unwrap();
                    (u128::from(old), u128::from(new))
                }
            };
            node.total_size = node
                .total_size
                .checked_sub(old)
                .unwrap()
                .checked_add(new)
                .unwrap();
            (old, new)
        }
        let hasher = tree.name_hasher.clone();
        let components = normal_components(std::path::Path::new(dir_path)).unwrap();
        descend(
            tree.root.as_mut().unwrap(),
            &components,
            &hasher,
            std::ffi::OsStr::new(name),
            new_size,
        );
    }

    /// 長さが同じ名前はすべて衝突する木。
    /// root(){ d(one=1,two=2,six=3 [長さ3で衝突], longer=40){ } }
    fn colliding_tree_with(files: &[(&str, u64)]) -> Tree {
        let hasher = colliding_hasher();
        let d = dir(&hasher, "d", files, false, vec![]);
        let root = dir(&hasher, "", &[("xy", 1000)], false, vec![d]);
        let mut tree = Tree::new();
        tree.apply_full(full_tree(root, hasher, &[]), false, T0)
            .unwrap();
        tree
    }

    #[test]
    fn colliding_names_keep_sizes_correct_through_the_tree() {
        init_logger();
        let tree = colliding_tree_with(&[("one", 1), ("two", 2), ("six", 3), ("longer", 40)]);
        assert_eq!(total_of(&tree, "d"), 46);
        assert_eq!(
            tree.find_dir(std::path::Path::new("d")).unwrap().own_size(),
            46
        );
        assert_eq!(total_of(&tree, ""), 1046);
        // "xy"（長さ 2）は "d" の中の名前とは別のディレクトリなので衝突しない
        assert_eq!(tree.root().unwrap().counts().hash_collision_count, 2);
        assert_eq!(tree.root().unwrap().counts().file_count, 5);
    }

    #[test]
    fn collided_file_size_change_and_removal_use_the_real_name() {
        init_logger();
        let mut tree = colliding_tree_with(&[("one", 1), ("two", 2), ("six", 3), ("longer", 40)]);
        // 衝突中のファイル "two" のサイズを変える
        change_file(&mut tree, "d", "two", Some(20));
        assert_eq!(total_of(&tree, "d"), 64);
        assert_eq!(total_of(&tree, ""), 1064);
        let expected = colliding_tree_with(&[("one", 1), ("two", 20), ("six", 3), ("longer", 40)]);
        assert_eq!(tree.root(), expected.root());
        // 衝突中のファイル "one" を削除する。他の 2 つのサイズは影響を受けない
        change_file(&mut tree, "d", "one", None);
        assert_eq!(total_of(&tree, "d"), 63);
        assert_eq!(total_of(&tree, ""), 1063);
        let expected = colliding_tree_with(&[("two", 20), ("six", 3), ("longer", 40)]);
        assert_eq!(tree.root(), expected.root());
        assert_eq!(tree.root().unwrap().counts().hash_collision_count, 1);
        // 残りも削除すると、エントリが消える
        change_file(&mut tree, "d", "two", None);
        change_file(&mut tree, "d", "six", None);
        assert_eq!(total_of(&tree, "d"), 40);
        assert_eq!(tree.root().unwrap().counts().hash_collision_count, 0);
        assert!(
            tree.find_dir(std::path::Path::new("d"))
                .unwrap()
                .files()
                .len()
                == 1
        );
    }

    #[test]
    fn single_entry_size_change_and_removal_propagate_to_ancestors() {
        init_logger();
        let hasher = NameHasher::new();
        let mut tree = scanned_tree(hasher);
        change_file(&mut tree, "a/b", "b1", Some(1));
        assert_eq!(total_of(&tree, "a/b"), 201);
        assert_eq!(total_of(&tree, ""), 217);
        change_file(&mut tree, "a/b", "b2", None);
        assert_eq!(total_of(&tree, "a/b"), 1);
        assert_eq!(total_of(&tree, "a"), 11);
        assert_eq!(total_of(&tree, ""), 17);
        assert_eq!(
            tree.find_dir(std::path::Path::new("a/b"))
                .unwrap()
                .own_size(),
            1
        );
    }

    #[test]
    fn partial_apply_with_collisions_matches_a_full_build() {
        init_logger();
        let hasher = colliding_hasher();
        let build = |sub_files: &[(&str, u64)]| {
            let sub = dir(&hasher, "sub", sub_files, false, vec![]);
            dir(&hasher, "", &[("p", 1), ("q", 2)], false, vec![sub])
        };
        let mut tree = Tree::new();
        tree.apply_full(
            full_tree(build(&[("m", 5), ("nn", 6)]), hasher.clone(), &[]),
            false,
            T0,
        )
        .unwrap();
        let new_sub = dir(
            &hasher,
            "sub",
            &[("u", 7), ("v", 8), ("ww", 9)],
            false,
            vec![],
        );
        tree.apply_partial(std::path::Path::new("sub"), new_sub, false, T0)
            .unwrap();
        let expected = build(&[("u", 7), ("v", 8), ("ww", 9)]);
        assert_eq!(tree.root(), Some(&expected));
        assert_eq!(tree.root().unwrap().counts().hash_collision_count, 2);
    }

    // ---- 部分スキャン後の結果が、同じ構造をフルで構築した結果と一致する性質 ----

    #[derive(Clone, Debug, Default)]
    struct ModelDir {
        files: std::collections::BTreeMap<u8, u64>,
        broken: bool,
    }

    /// ディレクトリのパス（各要素は名前の番号）から内容への表。ルートは空のパス。
    type Model = std::collections::BTreeMap<Vec<u8>, ModelDir>;

    /// （ディレクトリのパス, ファイル（名前の番号, サイズ）, エラーを付けるか）の並びから
    /// モデルを作る。途中のディレクトリは自動的に作る。
    type ModelEntry = (Vec<u8>, Option<(u8, u64)>, bool);

    fn model_from(entries: &[ModelEntry]) -> Model {
        let mut model = Model::new();
        model.insert(Vec::new(), ModelDir::default());
        for (path, file_entry, broken) in entries {
            for length in 0..=path.len() {
                model.entry(path[..length].to_vec()).or_default();
            }
            let node = model.get_mut(path).unwrap();
            if let Some((file_name, size)) = file_entry {
                node.files.insert(*file_name, *size);
            }
            if *broken {
                node.broken = true;
            }
        }
        model
    }

    fn model_dir_name(path: &[u8]) -> String {
        match path.last() {
            Some(index) => format!("d{index}"),
            None => String::new(),
        }
    }

    fn build_model_node(model: &Model, path: &[u8], hasher: &NameHasher) -> DirNode {
        let mut node = DirNode::new(os(&model_dir_name(path)), 1, path.len() as u64);
        let this = &model[path];
        let files: Vec<(std::ffi::OsString, u64)> = this
            .files
            .iter()
            .map(|(name, size)| (os(&format!("f{name}")), *size))
            .collect();
        node.set_files(hasher, &files).unwrap();
        if this.broken {
            node.record_stat_error(libc::EACCES);
        }
        for child_path in model.keys() {
            if child_path.len() == path.len() + 1 && child_path.starts_with(path) {
                node.add_child(build_model_node(model, child_path, hasher))
                    .unwrap();
            }
        }
        node.finalize().unwrap();
        node
    }

    fn path_of(indexes: &[u8]) -> std::path::PathBuf {
        indexes
            .iter()
            .map(|index| format!("d{index}"))
            .collect::<std::path::PathBuf>()
    }

    fn model_entry_strategy() -> impl proptest::strategy::Strategy<Value = ModelEntry> {
        (
            proptest::collection::vec(0u8..3, 0..4),
            proptest::option::of((0u8..5, 0u64..10_000)),
            proptest::bool::weighted(0.2),
        )
    }

    proptest::proptest! {
        #[test]
        fn partial_apply_matches_a_full_build(
            old_entries in proptest::collection::vec(model_entry_strategy(), 0..24),
            new_entries in proptest::collection::vec(model_entry_strategy(), 0..12),
            target_pick in proptest::prelude::any::<proptest::sample::Index>(),
            collide in proptest::prelude::any::<bool>(),
        ) {
            let hasher = if collide { colliding_hasher() } else { NameHasher::new() };
            let old_model = model_from(&old_entries);
            let targets: Vec<&Vec<u8>> = old_model.keys().collect();
            let target: Vec<u8> = targets[target_pick.index(targets.len())].clone();
            // 対象より下を new_entries で作り直したモデル
            let sub_model = model_from(&new_entries);
            let mut expected_model: Model = old_model
                .iter()
                .filter(|(path, _)| !(path.starts_with(&target) && path.len() > target.len()))
                .map(|(path, dir)| (path.clone(), dir.clone()))
                .collect();
            for (sub_path, sub_dir) in &sub_model {
                let mut path = target.clone();
                path.extend_from_slice(sub_path);
                expected_model.insert(path, sub_dir.clone());
            }
            let expected_root = build_model_node(&expected_model, &[], &hasher);

            let mut tree = Tree::new();
            tree.apply_full(
                FullScanTree {
                    root: build_model_node(&old_model, &[], &hasher),
                    exclude_patterns: patterns(&[]),
                    name_hasher: hasher.clone(),
                },
                false,
                T0,
            ).unwrap();
            // 部分スキャン側の部分木は、expected_model の target 以下から作る
            let new_subtree = build_model_node(&expected_model, &target, &hasher);
            tree.apply_partial(&path_of(&target), new_subtree, false, T0).unwrap();
            proptest::prop_assert_eq!(tree.root(), Some(&expected_root));
            proptest::prop_assert_eq!(tree.generation(), 2);
            let root = tree.root().unwrap();
            proptest::prop_assert_eq!(
                tree.snapshot_state() == SnapshotState::Complete,
                root.subtree_error_dir_count() == 0
            );
        }
    }
}
