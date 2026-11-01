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
/// 通常は単独で、サイズだけを持つ。同じディレクトリ内でハッシュが重なったときだけ
/// 「衝突」にして、実名とサイズの一覧で持つ。衝突中のファイルも実名で引けるので、
/// 削除やサイズ変更を正しく反映できる。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileEntry {
    /// 単独（ファイルのサイズ `st_size`）
    Single(u64),
    /// 衝突（2 件以上の、実名とサイズの一覧）
    Collided(Vec<CollidedFile>),
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
                let collided: Vec<CollidedFile> = group
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
        // 単独のときはサイズだけで、衝突したときだけ Vec を持つ（ハッシュ表の要素は
        // ハッシュ 8 バイトとエントリ）。
        assert!(std::mem::size_of::<FileEntry>() <= 24);
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
}
