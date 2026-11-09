//! Virtual file system over the game's `Data` folder: BSA archives in precedence order
//! plus loose files, with case-insensitive, slash-agnostic paths (docs/design/layer2.md
//! §2.5). Read-only: nothing here writes to the game install.
//!
//! Precedence, lowest first (docs/spec-notes/bsa.md, "Precedence"): the INI
//! `SArchiveList` archives in list order, then `Update.bsa`, then each active plugin's
//! archives (`<plugin stem> - *.bsa`, `<plugin stem>.bsa`) in load order, then loose files.
//! A later source replaces an earlier one's file with the same path. Checked against
//! in-game screenshots [observed]: `Update.bsa` overrides `Fallout - Meshes.bsa`, and a DLC
//! archive overrides the base archives. Still [hypothesis]: `Update.bsa` below the DLC
//! archives, DLC against DLC by load order, and loose files above every archive.

use fnv_bsa::Archive;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// `SArchiveList` of the shipped `Fallout_default.ini` and of the owner's `Fallout.ini`
/// [observed: both identical on FNV 1.4.0.525].
pub const DEFAULT_ARCHIVE_LIST: [&str; 6] = [
    "Fallout - Textures.bsa",
    "Fallout - Textures2.bsa",
    "Fallout - Meshes.bsa",
    "Fallout - Voices1.bsa",
    "Fallout - Sound.bsa",
    "Fallout - Misc.bsa",
];

/// The base game and DLC masters in their usual load order.
pub const DEFAULT_PLUGINS: [&str; 10] = [
    "FalloutNV.esm",
    "DeadMoney.esm",
    "HonestHearts.esm",
    "OldWorldBlues.esm",
    "LonesomeRoad.esm",
    "GunRunnersArsenal.esm",
    "ClassicPack.esm",
    "MercenaryPack.esm",
    "TribalPack.esm",
    "CaravanPack.esm",
];

#[derive(Debug, thiserror::Error)]
pub enum VfsError {
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("{0}: {1}")]
    Bsa(String, fnv_bsa::BsaError),
}

/// Environment variable that replaces the archive order of [`Vfs::open_default`]
/// (diagnostics only; see there).
pub const ARCHIVE_ORDER_ENV: &str = "NVF_ARCHIVE_ORDER";

static ARCHIVE_LIST: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Set the run's base archive list (an `SArchiveList` from NVForever's own INI, or
/// `fnvtool vfs order --archive-list`) in place of [`DEFAULT_ARCHIVE_LIST`], before any
/// archive is mounted (issue #1142: a mod setup that adds base archives). Once per run: a
/// second call is refused (`Err` with the list kept). An empty list is refused too.
pub fn set_archive_list(list: Vec<String>) -> Result<(), Vec<String>> {
    if list.is_empty() {
        return Err(list);
    }
    ARCHIVE_LIST.set(list)
}

/// The run's base archive list, lowest precedence first: the one set with
/// [`set_archive_list`], else [`DEFAULT_ARCHIVE_LIST`]. `Update.bsa` and the plugins'
/// own archives come after it ([`Vfs::archive_order`]).
pub fn archive_list() -> Vec<String> {
    match ARCHIVE_LIST.get() {
        Some(l) => l.clone(),
        None => DEFAULT_ARCHIVE_LIST.iter().map(|s| s.to_string()).collect(),
    }
}

/// Whether the run's base archive list was set ([`set_archive_list`]).
pub fn archive_list_overridden() -> bool {
    ARCHIVE_LIST.get().is_some()
}

/// Archive names of a comma-separated list (`SArchiveList` style), trimmed, empty items
/// dropped.
pub fn parse_archive_list(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Normalised form of a game path: lower case, backslashes, no leading separator, and a
/// run of separators taken as one. [observed: the Classic Fallout Weapons mod's
/// `Colt6520.nif` names `textures\\weapons\1handpistol\colt6520_anim.dds`, the file lies at
/// `textures\weapons\1handpistol\`, and the owner sees the gun textured in the original
/// game; hypothesis: the game reads the path through Windows' file calls, which take a
/// doubled separator as one (#1705)]
pub fn normalize(path: &str) -> String {
    let mut s = String::with_capacity(path.len());
    for c in path.chars() {
        let c = if c == '/' {
            '\\'
        } else {
            c.to_ascii_lowercase()
        };
        if c == '\\' && (s.is_empty() || s.ends_with('\\')) {
            continue;
        }
        s.push(c);
    }
    s
}

/// Map a texture path as written in a NIF to a VFS path: NIFs mostly store
/// `textures\...`, but some carry a leading `data\` or an absolute path that contains
/// `\textures\` [hypothesis: the engine strips everything before `textures\`].
pub fn texture_path(raw: &str) -> String {
    let p = normalize(raw);
    if p.starts_with("textures\\") {
        return p;
    }
    if let Some(i) = p.find("\\textures\\") {
        return p[i + 1..].to_string();
    }
    format!("textures\\{p}")
}

/// The path of the normal map that lies beside a texture by name: the same path with
/// `_n` before the extension (`…\ghoul\upperbodymale.dds` → `…\ghoul\upperbodymale_n.dds`);
/// the rest of the path is kept as written. `None` for a path without a file name and
/// extension. Whether that file exists is the caller's question ([`Vfs::exists`]).
/// [observed: every skin shape of the actor models names in texture slot 1 the file
/// named this way after its slot 0; hypothesis: the game finds the normal map of a skin
/// texture it puts in by this name, docs/spec-notes/actors.md §3.1]
pub fn normal_map_beside(texture: &str) -> Option<String> {
    let file_start = texture.rfind(['\\', '/']).map_or(0, |i| i + 1);
    let dot = file_start + texture[file_start..].rfind('.')?;
    if dot == file_start || dot + 1 == texture.len() {
        return None;
    }
    Some(format!("{}_n{}", &texture[..dot], &texture[dot..]))
}

#[derive(Debug, Clone, Copy)]
enum Location {
    Archive {
        archive: usize,
        folder: usize,
        file: usize,
    },
    Loose,
}

#[derive(Debug, Clone)]
struct DataFile {
    layer: usize,
    relative_path: PathBuf,
}

/// A case-insensitive, read-only view over data directories ordered from lowest to
/// highest priority. Directories merge; a later file replaces an earlier file at the
/// same virtual path regardless of path casing.
#[derive(Debug, Clone)]
pub struct VirtualDataDir {
    layers: Vec<PathBuf>,
    files: HashMap<String, DataFile>,
    skipped: Vec<(String, String)>,
}

impl VirtualDataDir {
    /// Build the merged view before discovering archives or indexing loose files.
    /// Unreadable directories and entries are recorded and skipped.
    pub fn new<P: AsRef<Path>>(layers: &[P]) -> VirtualDataDir {
        let layers: Vec<PathBuf> = layers.iter().map(|p| p.as_ref().to_path_buf()).collect();
        let mut files = HashMap::new();
        let mut skipped = Vec::new();
        for (layer, root) in layers.iter().enumerate() {
            let mut relative_files = Vec::new();
            walk(root, root, &mut relative_files, &mut skipped);
            relative_files.sort();
            for relative_path in relative_files {
                let key = normalize(&relative_path.to_string_lossy());
                files.insert(
                    key,
                    DataFile {
                        layer,
                        relative_path,
                    },
                );
            }
        }
        VirtualDataDir {
            layers,
            files,
            skipped,
        }
    }

    /// Data directories in precedence order, lowest priority first.
    pub fn layers(&self) -> &[PathBuf] {
        &self.layers
    }

    /// Physical path of the file that wins at a case-insensitive virtual path.
    pub fn resolve(&self, path: &str) -> Option<PathBuf> {
        let file = self.files.get(&normalize(path))?;
        Some(self.layers[file.layer].join(&file.relative_path))
    }

    /// Paths skipped while building the view because a directory or entry could not be read.
    pub fn skipped(&self) -> &[(String, String)] {
        &self.skipped
    }

    fn relative_path(&self, path: &str) -> Option<&Path> {
        self.files
            .get(&normalize(path))
            .map(|file| file.relative_path.as_path())
    }

    fn root_files(&self) -> impl Iterator<Item = &Path> {
        self.files.values().filter_map(|file| {
            (file.relative_path.components().count() == 1).then_some(file.relative_path.as_path())
        })
    }
}

pub struct Vfs {
    data_dir: VirtualDataDir,
    archives: Vec<(String, Mutex<Archive>)>,
    index: HashMap<String, Location>,
    /// The highest archive copy of each loose path that also sits in an archive, which
    /// the loose file hides from [`Vfs::read`] ([`Vfs::read_archived`], #975).
    under_loose: HashMap<String, Location>,
    /// Archives and folders that could not be read, with why: skipped, not fatal (a
    /// broken mod must not stop the game). Kept so a caller whose log is not up yet can
    /// report them later.
    skipped: Vec<(String, String)>,
    /// Archives that cannot be opened or whose tables are damaged ([`fnv_bsa::check`]),
    /// with why. The game will not start with one (engine `main.rs`); tools go on with
    /// what reads.
    broken: Vec<(String, String)>,
}

impl Vfs {
    /// Archive file names in precedence order for `plugins` (lowest first).
    pub fn archive_order(data_dir: &Path, archive_list: &[&str], plugins: &[&str]) -> Vec<String> {
        Vfs::archive_order_in(data_dir, None, archive_list, plugins)
    }

    /// [`Vfs::archive_order`], with the plugins' archives also looked for in NVForever's
    /// mods folder `mods`, beside the plugins it holds (docs/design/editor.md §2.2 item 9).
    /// One found there is named by its full path ([`Vfs::open`] joins a relative name to
    /// `data_dir`); a name in both folders is the mods folder's, as a plugin is
    /// (`fnv_install::plugins::plugin_path`). The base list and `Update.bsa` come from
    /// `data_dir` only.
    pub fn archive_order_in(
        data_dir: &Path,
        mods: Option<&Path>,
        archive_list: &[&str],
        plugins: &[&str],
    ) -> Vec<String> {
        let data_dir = VirtualDataDir::new(&[data_dir]);
        Vfs::archive_order_virtual(&data_dir, mods, archive_list, plugins)
    }

    /// Archive order from an overlaid data directory.
    pub fn archive_order_layers<P: AsRef<Path>>(
        data_dirs: &[P],
        archive_list: &[&str],
        plugins: &[&str],
    ) -> Vec<String> {
        let data_dir = VirtualDataDir::new(data_dirs);
        Vfs::archive_order_virtual(&data_dir, None, archive_list, plugins)
    }

    /// Archive order from an already-built virtual data directory, optionally including
    /// archives beside plugins in NVForever's mods folder.
    pub fn archive_order_virtual(
        data_dir: &VirtualDataDir,
        mods: Option<&Path>,
        archive_list: &[&str],
        plugins: &[&str],
    ) -> Vec<String> {
        let bsas = |dir: &Path| -> Vec<String> {
            std::fs::read_dir(dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|n| n.to_ascii_lowercase().ends_with(".bsa"))
                        .collect()
                })
                .unwrap_or_default()
        };
        let present: Vec<String> = data_dir
            .root_files()
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("bsa")))
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        let in_mods: Vec<String> = mods.map(bsas).unwrap_or_default();
        // Each plugin archive's name, and what `Vfs::open` is given for it.
        let mut owned: Vec<(String, String)> = present
            .iter()
            .filter(|n| !in_mods.iter().any(|m| m.eq_ignore_ascii_case(n)))
            .map(|n| (n.clone(), n.clone()))
            .collect();
        if let Some(m) = mods {
            owned.extend(
                in_mods
                    .iter()
                    .map(|n| (n.clone(), m.join(n).to_string_lossy().into_owned())),
            );
        }
        let find = |name: &str| {
            let key = normalize(name);
            data_dir
                .relative_path(&key)
                .filter(|p| p.components().count() == 1)
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned())
        };
        let mut order: Vec<String> = Vec::new();
        let push = |n: Option<String>, order: &mut Vec<String>| {
            if let Some(n) = n {
                if !order.iter().any(|o| o.eq_ignore_ascii_case(&n)) {
                    order.push(n);
                }
            }
        };
        for a in archive_list {
            push(find(a), &mut order);
        }
        push(find("Update.bsa"), &mut order);
        let stems: Vec<String> = plugins
            .iter()
            .map(|p| {
                p.rsplit_once('.')
                    .map_or(*p, |(s, _)| s)
                    .to_ascii_lowercase()
            })
            .collect();
        let claims = |stem: &str, name: &str| {
            let l = name.to_ascii_lowercase();
            l == format!("{stem}.bsa") || l.starts_with(&format!("{stem} - "))
        };
        for stem in &stems {
            // An archive several plugins' stems match belongs to the longest of them:
            // `Foo - Bar.bsa` is `Foo - Bar.esp`'s, not `Foo.esp`'s (#996).
            let mut own: Vec<&(String, String)> = owned
                .iter()
                .filter(|(n, _)| {
                    claims(stem, n) && !stems.iter().any(|o| o.len() > stem.len() && claims(o, n))
                })
                .collect();
            own.sort_by_key(|(n, _)| n.to_ascii_lowercase());
            for (_, open_as) in own {
                push(Some(open_as.clone()), &mut order);
            }
        }
        order
    }

    /// Open the archives (lowest precedence first) and index loose files under `data_dir`.
    pub fn open(data_dir: &Path, archives: &[String]) -> Result<Vfs, VfsError> {
        Vfs::open_virtual(VirtualDataDir::new(&[data_dir]), archives)
    }

    /// Open archives (lowest precedence first) and loose files from overlaid data
    /// directories ordered from lowest to highest priority.
    pub fn open_layers<P: AsRef<Path>>(
        data_dirs: &[P],
        archives: &[String],
    ) -> Result<Vfs, VfsError> {
        Vfs::open_virtual(VirtualDataDir::new(data_dirs), archives)
    }

    /// Open archives and loose files from an already-built virtual data directory.
    pub fn open_virtual(data_dir: VirtualDataDir, archives: &[String]) -> Result<Vfs, VfsError> {
        let mut vfs = Vfs {
            data_dir,
            archives: Vec::new(),
            index: HashMap::new(),
            under_loose: HashMap::new(),
            skipped: Vec::new(),
            broken: Vec::new(),
        };
        vfs.skipped.extend(vfs.data_dir.skipped().iter().cloned());
        for name in archives {
            // A bad or wrong-version archive is skipped; the others still load.
            let archive_path = vfs
                .data_dir
                .resolve(name)
                .unwrap_or_else(|| vfs.data_dir().join(name));
            let archive_name = vfs
                .data_dir
                .relative_path(name)
                .map_or_else(|| name.clone(), |p| p.to_string_lossy().into_owned());
            let a = match Archive::open(&archive_path) {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(archive = %archive_name, error = %e, "archive skipped: cannot be read");
                    vfs.skipped.push((archive_name.clone(), e.to_string()));
                    vfs.broken.push((archive_name, e.to_string()));
                    continue;
                }
            };
            if let Err(e) = a.check_tables() {
                tracing::warn!(archive = %archive_name, error = %e, "archive damaged: its tables point past the end");
                vfs.broken.push((archive_name.clone(), e.to_string()));
            }
            let ai = vfs.archives.len();
            for e in a.entries() {
                vfs.index.insert(
                    normalize(&e.path()),
                    Location::Archive {
                        archive: ai,
                        folder: e.folder_index,
                        file: e.file_index,
                    },
                );
            }
            vfs.archives.push((archive_name, Mutex::new(a)));
        }
        for key in vfs.data_dir.files.keys() {
            // Plugins and archives at the top level are not part of the asset namespace.
            if !key.contains('\\') {
                continue;
            }
            if let Some(under @ Location::Archive { .. }) =
                vfs.index.insert(key.clone(), Location::Loose)
            {
                vfs.under_loose.insert(key.clone(), under);
            }
        }
        Ok(vfs)
    }

    /// Default setup: the INI archive list, `Update.bsa`, and the base + DLC plugins.
    ///
    /// Diagnostic override: when the environment variable [`ARCHIVE_ORDER_ENV`] is set, its
    /// comma-separated archive names (lowest precedence first, like `SArchiveList`) replace
    /// that order. It exists to render the same scene under competing precedence rules and
    /// compare them with the original game (docs/spec-notes/bsa.md, "Precedence").
    pub fn open_default(data_dir: &Path) -> Result<Vfs, VfsError> {
        Vfs::open_default_layers(&[data_dir])
    }

    /// [`Vfs::open_default`] over data directories ordered from lowest to highest priority.
    pub fn open_default_layers<P: AsRef<Path>>(data_dirs: &[P]) -> Result<Vfs, VfsError> {
        Vfs::open_for_plugins_layers(data_dirs, &DEFAULT_PLUGINS)
    }

    /// [`Vfs::open_default`] for the plugins actually loaded, in load order: a mod's
    /// archives (`<mod> - *.bsa`, `<mod>.bsa`) come above the base game's and DLC's when
    /// its plugin loads after them, and loose files stay above every archive. The
    /// [`ARCHIVE_ORDER_ENV`] override applies as there.
    pub fn open_for_plugins<S: AsRef<str>>(
        data_dir: &Path,
        plugins: &[S],
    ) -> Result<Vfs, VfsError> {
        Vfs::open_for_plugins_in(data_dir, None, plugins)
    }

    /// [`Vfs::open_for_plugins`] with the archives beside the plugins in NVForever's mods
    /// folder `mods` too ([`Vfs::archive_order_in`]).
    pub fn open_for_plugins_in<S: AsRef<str>>(
        data_dir: &Path,
        mods: Option<&Path>,
        plugins: &[S],
    ) -> Result<Vfs, VfsError> {
        Vfs::open_for_plugins_layers_in(&[data_dir], mods, plugins)
    }

    /// [`Vfs::open_for_plugins`] over data directories ordered from lowest to highest
    /// priority. Archive discovery and loose-file indexing use the same merged view.
    pub fn open_for_plugins_layers<P: AsRef<Path>, S: AsRef<str>>(
        data_dirs: &[P],
        plugins: &[S],
    ) -> Result<Vfs, VfsError> {
        Vfs::open_for_plugins_layers_in(data_dirs, None, plugins)
    }

    /// [`Vfs::open_for_plugins_layers`] with plugin archives also discovered beside
    /// plugins in NVForever's mods folder.
    pub fn open_for_plugins_layers_in<P: AsRef<Path>, S: AsRef<str>>(
        data_dirs: &[P],
        mods: Option<&Path>,
        plugins: &[S],
    ) -> Result<Vfs, VfsError> {
        let data_dir = VirtualDataDir::new(data_dirs);
        let archives = Vfs::order_for_plugins_virtual(&data_dir, mods, plugins).0;
        Vfs::open_virtual(data_dir, &archives)
    }

    /// The archives [`Vfs::open_for_plugins`] mounts, lowest precedence first, and
    /// whether they came from the [`ARCHIVE_ORDER_ENV`] override (`fnvtool vfs order`).
    /// The base list is [`archive_list`]'s: an `SArchiveList` set for the run, else the
    /// shipped default.
    pub fn order_for_plugins<S: AsRef<str>>(data_dir: &Path, plugins: &[S]) -> (Vec<String>, bool) {
        Vfs::order_for_plugins_in(data_dir, None, plugins)
    }

    /// [`Vfs::order_for_plugins`] with the mods folder's archives ([`Vfs::archive_order_in`]).
    pub fn order_for_plugins_in<S: AsRef<str>>(
        data_dir: &Path,
        mods: Option<&Path>,
        plugins: &[S],
    ) -> (Vec<String>, bool) {
        let data_dir = VirtualDataDir::new(&[data_dir]);
        Vfs::order_for_plugins_virtual(&data_dir, mods, plugins)
    }

    /// [`Vfs::order_for_plugins`] using overlaid data directories.
    pub fn order_for_plugins_layers<P: AsRef<Path>, S: AsRef<str>>(
        data_dirs: &[P],
        plugins: &[S],
    ) -> (Vec<String>, bool) {
        let data_dir = VirtualDataDir::new(data_dirs);
        Vfs::order_for_plugins_virtual(&data_dir, None, plugins)
    }

    /// [`Vfs::order_for_plugins_in`] using an already-built virtual data directory.
    pub fn order_for_plugins_virtual<S: AsRef<str>>(
        data_dir: &VirtualDataDir,
        mods: Option<&Path>,
        plugins: &[S],
    ) -> (Vec<String>, bool) {
        match std::env::var(ARCHIVE_ORDER_ENV) {
            Ok(list) if !list.trim().is_empty() => (parse_archive_list(&list), true),
            _ => {
                let plugins: Vec<&str> = plugins.iter().map(AsRef::as_ref).collect();
                let list = archive_list();
                let list: Vec<&str> = list.iter().map(String::as_str).collect();
                (
                    Vfs::archive_order_virtual(data_dir, mods, &list, &plugins),
                    false,
                )
            }
        }
    }

    pub fn archives(&self) -> impl Iterator<Item = &str> {
        self.archives.iter().map(|(n, _)| n.as_str())
    }

    /// The first, lowest-priority data directory (its top-level files, `Credits.txt`, are
    /// not in the asset namespace).
    pub fn data_dir(&self) -> &Path {
        self.data_dir
            .layers
            .first()
            .map_or_else(|| Path::new(""), PathBuf::as_path)
    }

    /// Archives and loose folders skipped because they could not be read: (name, why).
    pub fn skipped(&self) -> &[(String, String)] {
        &self.skipped
    }

    /// Archives that cannot be opened or whose header or tables are damaged: (name, why).
    pub fn broken_archives(&self) -> &[(String, String)] {
        &self.broken
    }

    pub fn exists(&self, path: &str) -> bool {
        self.index.contains_key(&normalize(path))
    }

    /// Which source provides `path`: an archive name, or `"loose"`.
    pub fn source_of(&self, path: &str) -> Option<&str> {
        match self.index.get(&normalize(path))? {
            Location::Archive { archive, .. } => Some(&self.archives[*archive].0),
            Location::Loose => Some("loose"),
        }
    }

    /// The file on disk of a loose `path` (for reading a large file as a stream); `None`
    /// when no source has it or an archive does.
    pub fn loose_path(&self, path: &str) -> Option<PathBuf> {
        let key = normalize(path);
        match self.index.get(&key)? {
            Location::Loose => self.data_dir.resolve(&key),
            Location::Archive { .. } => None,
        }
    }

    /// Decoded contents of `path`, or `None` when no source has it.
    pub fn read(&self, path: &str) -> Option<std::io::Result<Vec<u8>>> {
        let key = normalize(path);
        Some(self.read_at(&key, *self.index.get(&key)?))
    }

    /// Decoded contents of the highest archive copy of `path`, also when a loose file
    /// replaces it; `None` when no archive has it. Menu files are compared with the
    /// copy a loose replacement hides (docs/design/ui-tile-tree.md, #975).
    pub fn read_archived(&self, path: &str) -> Option<std::io::Result<Vec<u8>>> {
        let key = normalize(path);
        let at = match *self.index.get(&key)? {
            Location::Loose => *self.under_loose.get(&key)?,
            at => at,
        };
        Some(self.read_at(&key, at))
    }

    fn read_at(&self, key: &str, at: Location) -> std::io::Result<Vec<u8>> {
        match at {
            Location::Loose => {
                let path = self.data_dir.resolve(key).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::NotFound, key.to_string())
                })?;
                std::fs::read(path)
            }
            Location::Archive {
                archive,
                folder,
                file,
            } => {
                // The lock covers only the seek and read of the stored bytes; inflating
                // happens after it is let go, so workers reading the same archive do not
                // queue behind each other's decompression (#997).
                let stored = {
                    let mut a = self.archives[archive]
                        .1
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    let r = a.folders[folder].files[file].clone();
                    a.read_stored(&r)
                };
                stored
                    .and_then(|b| b.decode())
                    .map_err(std::io::Error::other)
            }
        }
    }

    /// Every path (normalised) for which `pred` holds, sorted.
    pub fn paths(&self, pred: impl Fn(&str) -> bool) -> Vec<String> {
        let mut v: Vec<String> = self.index.keys().filter(|k| pred(k)).cloned().collect();
        v.sort();
        v
    }
}

/// Every loose file under `dir`. A folder or entry that cannot be read is skipped (and
/// noted in `skipped`); the rest of the walk goes on.
fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>, skipped: &mut Vec<(String, String)>) {
    let mut skip = |p: &Path, e: std::io::Error| {
        tracing::warn!(path = %p.display(), error = %e, "loose files skipped: cannot be read");
        skipped.push((p.display().to_string(), e.to_string()));
    };
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => return skip(dir, e),
    };
    let mut subdirs = Vec::new();
    for e in rd {
        let e = match e {
            Ok(e) => e,
            Err(err) => {
                skip(dir, err);
                continue;
            }
        };
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => subdirs.push(p),
            Ok(_) => {
                if let Ok(rel) = p.strip_prefix(root) {
                    out.push(rel.to_path_buf());
                }
            }
            Err(err) => skip(&p, err),
        }
    }
    for p in subdirs {
        walk(root, &p, out, skipped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The install check (fnv-install) asks for every archive of the default list and
    /// knows the same add-on masters.
    #[test]
    fn install_check_covers_default_archives() {
        for a in DEFAULT_ARCHIVE_LIST {
            assert!(fnv_install::REQUIRED_DATA_FILES.contains(&a), "{a}");
        }
        assert_eq!(fnv_install::REQUIRED_DATA_FILES[0], DEFAULT_PLUGINS[0]);
        assert_eq!(fnv_install::ADDON_MASTERS[..], DEFAULT_PLUGINS[1..]);
    }

    #[test]
    fn paths_normalise() {
        assert_eq!(normalize("/Meshes/Clutter/X.NIF"), "meshes\\clutter\\x.nif");
        assert_eq!(
            texture_path("Textures/Clutter/a.dds"),
            "textures\\clutter\\a.dds"
        );
        assert_eq!(texture_path("data\\textures\\a.dds"), "textures\\a.dds");
        assert_eq!(
            texture_path("c:\\proj\\data\\textures\\b\\a.dds"),
            "textures\\b\\a.dds"
        );
        assert_eq!(texture_path("clutter\\a.dds"), "textures\\clutter\\a.dds");
        // A doubled separator is one (#1705): mod meshes name such paths.
        assert_eq!(
            texture_path("textures\\\\weapons\\1HandPistol\\a.dds"),
            "textures\\weapons\\1handpistol\\a.dds"
        );
        assert_eq!(
            normalize("\\\\Meshes//Clutter\\/X.NIF"),
            "meshes\\clutter\\x.nif"
        );
        assert_eq!(
            texture_path("data\\\\textures\\\\b\\a.dds"),
            "textures\\b\\a.dds"
        );
    }

    #[test]
    fn the_normal_map_beside_a_texture_has_n_before_the_extension() {
        assert_eq!(
            normal_map_beside("textures\\characters\\ghoul\\upperbodymale.dds").as_deref(),
            Some("textures\\characters\\ghoul\\upperbodymale_n.dds")
        );
        // The path is kept as written (case, separators, no `textures\` in front).
        assert_eq!(
            normal_map_beside("Characters/RaiderFemale/HandFemale.DDS").as_deref(),
            Some("Characters/RaiderFemale/HandFemale_n.DDS")
        );
        assert_eq!(
            normal_map_beside("headhuman.dds").as_deref(),
            Some("headhuman_n.dds")
        );
        // Only the last dot of the file name counts, and dots in folder names do not.
        assert_eq!(
            normal_map_beside("a.b\\head.old.dds").as_deref(),
            Some("a.b\\head.old_n.dds")
        );
        assert_eq!(
            normal_map_beside("textures\\characters\\facemods\\falloutnv.esm\\x"),
            None
        );
        // No file name or no extension: no sibling.
        assert_eq!(normal_map_beside(""), None);
        assert_eq!(normal_map_beside("textures\\characters\\male\\"), None);
        assert_eq!(normal_map_beside("textures\\characters\\male\\.dds"), None);
        assert_eq!(
            normal_map_beside("textures\\characters\\male\\headhuman."),
            None
        );
        assert_eq!(
            normal_map_beside("textures\\characters\\male\\headhuman"),
            None
        );
    }

    #[test]
    fn loose_files_and_order() {
        let dir = std::env::temp_dir().join(format!("nvf-vfs-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Meshes/Sub")).unwrap();
        std::fs::write(dir.join("Meshes/Sub/A.nif"), b"loose").unwrap();
        for n in [
            "Fallout - Meshes.bsa",
            "Update.bsa",
            "DeadMoney - Main.bsa",
            "DeadMoney - Sounds.bsa",
            "Other.bsa",
        ] {
            std::fs::write(dir.join(n), b"").unwrap();
        }
        let order = Vfs::archive_order(
            &dir,
            &DEFAULT_ARCHIVE_LIST,
            &["FalloutNV.esm", "DeadMoney.esm"],
        );
        assert_eq!(
            order,
            [
                "Fallout - Meshes.bsa",
                "Update.bsa",
                "DeadMoney - Main.bsa",
                "DeadMoney - Sounds.bsa"
            ]
        );
        let vfs = Vfs::open(&dir, &[]).unwrap();
        assert!(vfs.exists("meshes/sub/a.NIF"));
        assert_eq!(vfs.source_of("MESHES\\SUB\\A.nif"), Some("loose"));
        assert_eq!(vfs.read("meshes\\sub\\a.nif").unwrap().unwrap(), b"loose");
        assert!(!vfs.exists("fallout - meshes.bsa"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn data_directory_layers_merge_case_insensitively_and_shadow_lower_files() {
        let root =
            std::env::temp_dir().join(format!("nvf-vfs-overlay-loose-{}", std::process::id()));
        let low = root.join("low");
        let high = root.join("high");
        std::fs::create_dir_all(low.join("Meshes/Weapons")).unwrap();
        std::fs::create_dir_all(high.join("mEsHeS/wEaPoNs")).unwrap();
        std::fs::create_dir_all(high.join("MESHES/Other")).unwrap();
        std::fs::write(low.join("Meshes/Weapons/Shared.NIF"), b"low").unwrap();
        std::fs::write(low.join("Meshes/Weapons/OnlyLow.NIF"), b"low only").unwrap();
        std::fs::write(high.join("mEsHeS/wEaPoNs/sHaReD.nif"), b"high").unwrap();
        std::fs::write(high.join("MESHES/Other/New.NIF"), b"new").unwrap();
        std::fs::write(high.join("Credits.txt"), b"not an asset").unwrap();

        let layers = [low.clone(), high.clone()];
        let view = VirtualDataDir::new(&layers);
        assert_eq!(view.layers(), layers);
        assert_eq!(
            view.resolve("meshes\\weapons\\SHARED.nif"),
            Some(high.join("mEsHeS/wEaPoNs/sHaReD.nif"))
        );

        let vfs = Vfs::open_virtual(view, &[]).unwrap();
        assert_eq!(
            vfs.read("MESHES/WEAPONS/shared.NIF").unwrap().unwrap(),
            b"high"
        );
        assert_eq!(
            vfs.read("meshes/weapons/onlylow.nif").unwrap().unwrap(),
            b"low only"
        );
        assert_eq!(vfs.read("meshes/other/new.nif").unwrap().unwrap(), b"new");
        assert_eq!(
            vfs.loose_path("Meshes/Weapons/Shared.nif"),
            Some(high.join("mEsHeS/wEaPoNs/sHaReD.nif"))
        );
        assert_eq!(vfs.source_of("meshes\\weapons\\shared.nif"), Some("loose"));
        assert!(!vfs.exists("Credits.txt"));
        let shared_paths = vfs.paths(|path| path == "meshes\\weapons\\shared.nif");
        assert_eq!(shared_paths, ["meshes\\weapons\\shared.nif"]);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn archive_discovery_and_reads_use_the_overlaid_data_directory() {
        use fnv_bsa::{encode_block, flags, write_archive, WriteFile, WriteFolder};

        let root =
            std::env::temp_dir().join(format!("nvf-vfs-overlay-archives-{}", std::process::id()));
        let low = root.join("low");
        let high = root.join("high");
        std::fs::create_dir_all(low.join("Meshes")).unwrap();
        std::fs::create_dir_all(high.join("mEsHeS")).unwrap();
        std::fs::write(low.join("Meshes/Overlay.NIF"), b"loose").unwrap();

        let write_test_archive = |path: &Path, contents: &[u8]| {
            let file = |name: &[u8]| WriteFile {
                name: name.to_vec(),
                size_flag_bits: 0,
                data: encode_block(contents, false, None),
            };
            let mut bytes = Vec::new();
            write_archive(
                &mut bytes,
                flags::DIR_NAMES | flags::FILE_NAMES,
                0x1,
                0,
                &[WriteFolder {
                    name: b"Meshes".to_vec(),
                    files: vec![file(b"Overlay.NIF"), file(b"ArchiveOnly.NIF")],
                }],
            )
            .unwrap();
            std::fs::write(path, bytes).unwrap();
        };
        write_test_archive(&low.join("Fallout - Meshes.bsa"), b"low archive");
        write_test_archive(&high.join("fAllout - mEsHeS.BSA"), b"high archive");
        std::fs::write(high.join("DeadMoney - Main.bsa"), b"").unwrap();

        let layers = [low.clone(), high.clone()];
        let view = VirtualDataDir::new(&layers);
        let order = Vfs::archive_order_virtual(&view, None, &["FALLOUT - MESHES.BSA"], &[]);
        assert_eq!(order, ["fAllout - mEsHeS.BSA"]);
        assert_eq!(
            Vfs::archive_order_virtual(
                &view,
                None,
                &["FALLOUT - MESHES.BSA"],
                &["FalloutNV.esm", "DeadMoney.esm"]
            ),
            ["fAllout - mEsHeS.BSA", "DeadMoney - Main.bsa"]
        );

        let vfs = Vfs::open_virtual(view, &order).unwrap();
        assert_eq!(vfs.archives().collect::<Vec<_>>(), ["fAllout - mEsHeS.BSA"]);
        assert_eq!(vfs.source_of("meshes/overlay.nif"), Some("loose"));
        assert_eq!(vfs.read("MESHES/OVERLAY.NIF").unwrap().unwrap(), b"loose");
        assert_eq!(
            vfs.read_archived("meshes/overlay.nif").unwrap().unwrap(),
            b"high archive"
        );
        assert_eq!(
            vfs.source_of("meshes/archiveonly.nif"),
            Some("fAllout - mEsHeS.BSA")
        );
        assert_eq!(
            vfs.read("Meshes/ArchiveOnly.NIF").unwrap().unwrap(),
            b"high archive"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A broken archive (here an empty file) is skipped, not fatal here; loose files still load.
    #[test]
    fn a_bad_archive_is_skipped() {
        let dir = std::env::temp_dir().join(format!("nvf-vfs-bad-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Meshes")).unwrap();
        std::fs::write(dir.join("Meshes/A.nif"), b"loose").unwrap();
        std::fs::write(dir.join("Broken.bsa"), b"").unwrap();
        let vfs = Vfs::open(&dir, &["Broken.bsa".to_string(), "Missing.bsa".to_string()]).unwrap();
        assert_eq!(vfs.archives().count(), 0);
        assert_eq!(vfs.skipped().len(), 2);
        // Both are named for the start-up check, with why.
        let broken: Vec<&str> = vfs
            .broken_archives()
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(broken, ["Broken.bsa", "Missing.bsa"]);
        assert!(
            vfs.broken_archives()[0].1.contains("0 bytes"),
            "{:?}",
            vfs.broken_archives()
        );
        assert!(vfs.exists("meshes/a.nif"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Files read through the VFS from an archive (compressed or not, names embedded or
    /// not) decode to the bytes stored, also when many threads read one archive at once.
    #[test]
    fn archive_reads_decode_the_same_bytes_from_many_threads() {
        use fnv_bsa::{
            encode_block, flags, write_archive, WriteFile, WriteFolder, SIZE_COMPRESSION_TOGGLE,
        };
        let dir = std::env::temp_dir().join(format!("nvf-vfs-bsa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let contents: Vec<Vec<u8>> = (0..24u32)
            .map(|i| (0..(i * 997 + 3)).map(|j| (j * 31 + i) as u8).collect())
            .collect();
        for (n, embed) in [("Plain.bsa", false), ("Named.bsa", true)] {
            let files = contents
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let name = format!("f{i}.nif").into_bytes();
                    let path = format!("meshes\\t\\f{i}.nif").into_bytes();
                    // Every other file compressed: the toggle against an uncompressed default.
                    let compress = i % 2 == 0;
                    WriteFile {
                        name,
                        size_flag_bits: if compress { SIZE_COMPRESSION_TOGGLE } else { 0 },
                        data: encode_block(c, compress, embed.then_some(&path[..])),
                    }
                })
                .collect();
            let mut bytes = Vec::new();
            let archive_flags = flags::DIR_NAMES
                | flags::FILE_NAMES
                | if embed { flags::EMBED_FILE_NAMES } else { 0 };
            write_archive(
                &mut bytes,
                archive_flags,
                0x1,
                0,
                &[WriteFolder {
                    name: b"meshes\\t".to_vec(),
                    files,
                }],
            )
            .unwrap();
            std::fs::write(dir.join(n), &bytes).unwrap();
        }
        for n in ["Plain.bsa", "Named.bsa"] {
            let vfs = Vfs::open(&dir, &[n.to_string()]).unwrap();
            assert_eq!(vfs.source_of("meshes\\t\\f0.nif"), Some(n));
            std::thread::scope(|s| {
                for t in 0..8 {
                    let (vfs, contents) = (&vfs, &contents);
                    s.spawn(move || {
                        for k in 0..contents.len() * 4 {
                            let i = (k * 7 + t) % contents.len();
                            assert_eq!(
                                vfs.read(&format!("meshes/t/f{i}.nif")).unwrap().unwrap(),
                                contents[i],
                                "{n} f{i}"
                            );
                        }
                    });
                }
            });
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A loose file hides an archive's copy from `read` but not from `read_archived`,
    /// which reads the highest archive's (#975); a path no archive has reads nothing there.
    #[test]
    fn the_archived_copy_under_a_loose_file_still_reads() {
        use fnv_bsa::{encode_block, flags, write_archive, WriteFile, WriteFolder};
        let dir = std::env::temp_dir().join(format!("nvf-vfs-under-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Menus/Main")).unwrap();
        std::fs::write(dir.join("Menus/Main/HUD_Main_Menu.xml"), b"loose").unwrap();
        std::fs::write(dir.join("Menus/Main/new.xml"), b"new").unwrap();
        for (n, text) in [("Low.bsa", b"low"), ("High.bsa", b"top")] {
            let file = |name: &str, data: &[u8]| WriteFile {
                name: name.as_bytes().to_vec(),
                size_flag_bits: 0,
                data: encode_block(data, false, None),
            };
            let files = vec![file("hud_main_menu.xml", text), file("only.xml", text)];
            let mut bytes = Vec::new();
            write_archive(
                &mut bytes,
                flags::DIR_NAMES | flags::FILE_NAMES,
                0x1,
                0,
                &[WriteFolder {
                    name: b"menus\\main".to_vec(),
                    files,
                }],
            )
            .unwrap();
            std::fs::write(dir.join(n), &bytes).unwrap();
        }
        let vfs = Vfs::open(&dir, &["Low.bsa".to_string(), "High.bsa".to_string()]).unwrap();
        assert_eq!(
            vfs.source_of("menus\\main\\hud_main_menu.xml"),
            Some("loose")
        );
        assert_eq!(
            vfs.read("menus/main/hud_main_menu.xml").unwrap().unwrap(),
            b"loose"
        );
        assert_eq!(
            vfs.read_archived("Menus/Main/HUD_Main_Menu.xml")
                .unwrap()
                .unwrap(),
            b"top"
        );
        // Not loose: the archived copy is the one that wins.
        assert_eq!(
            vfs.read_archived("menus\\main\\only.xml").unwrap().unwrap(),
            b"top"
        );
        assert!(vfs.read_archived("menus\\main\\new.xml").is_none());
        assert!(vfs.read_archived("menus\\main\\absent.xml").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mod_archives_follow_their_plugins() {
        let dir = std::env::temp_dir().join(format!("nvf-vfs-mods-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for n in [
            "Fallout - Meshes.bsa",
            "Update.bsa",
            "DeadMoney - Main.bsa",
            "MyMod.bsa",
            "MyMod - Textures.bsa",
            "Unused.bsa",
        ] {
            std::fs::write(dir.join(n), b"").unwrap();
        }
        // Base archives, then each plugin's own archives in load order; an archive no
        // loaded plugin names stays out.
        let order = Vfs::archive_order(
            &dir,
            &DEFAULT_ARCHIVE_LIST,
            &["FalloutNV.esm", "DeadMoney.esm", "MyMod.esp"],
        );
        assert_eq!(
            order,
            [
                "Fallout - Meshes.bsa",
                "Update.bsa",
                "DeadMoney - Main.bsa",
                "MyMod - Textures.bsa",
                "MyMod.bsa"
            ]
        );
        // Without the plugin its archives are not read.
        let order = Vfs::archive_order(&dir, &DEFAULT_ARCHIVE_LIST, &DEFAULT_PLUGINS);
        assert!(!order.iter().any(|n| n.starts_with("MyMod")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A plugin in NVForever's mods folder brings the archives beside it, named by their
    /// full path; one in both folders is the mods folder's copy.
    #[test]
    fn mods_folder_archives_follow_their_plugins() {
        let root = std::env::temp_dir().join(format!("nvf-vfs-modsdir-{}", std::process::id()));
        let (data, mods) = (root.join("Data"), root.join("mods"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&mods).unwrap();
        for n in ["Fallout - Meshes.bsa", "Update.bsa", "Both.bsa"] {
            std::fs::write(data.join(n), b"").unwrap();
        }
        for n in [
            "Edits.bsa",
            "Edits - Textures.bsa",
            "Both.bsa",
            "Update.bsa",
            "Stray.bsa",
        ] {
            std::fs::write(mods.join(n), b"").unwrap();
        }
        let full = |n: &str| mods.join(n).to_string_lossy().into_owned();
        let order = Vfs::archive_order_in(
            &data,
            Some(&mods),
            &DEFAULT_ARCHIVE_LIST,
            &["FalloutNV.esm", "Both.esp", "Edits.esp"],
        );
        // The base list and Update.bsa only from Data; no plugin claims Stray.bsa.
        assert_eq!(
            order,
            [
                "Fallout - Meshes.bsa".to_string(),
                "Update.bsa".into(),
                full("Both.bsa"),
                full("Edits - Textures.bsa"),
                full("Edits.bsa")
            ]
        );
        // Without a mods folder, as before.
        assert_eq!(
            Vfs::archive_order_in(
                &data,
                None,
                &DEFAULT_ARCHIVE_LIST,
                &["FalloutNV.esm", "Both.esp", "Edits.esp"]
            ),
            ["Fallout - Meshes.bsa", "Update.bsa", "Both.bsa"]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// `Foo - Bar.bsa` matches both `Foo.esp` ("Foo - *") and `Foo - Bar.esp`: it takes
    /// the longer stem's place, after `Foo - Bar.esp`'s other archives load order puts
    /// later (#996).
    #[test]
    fn an_archive_belongs_to_the_longest_plugin_stem() {
        let dir = std::env::temp_dir().join(format!("nvf-vfs-stems-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["Foo.bsa", "Foo - Main.bsa", "Foo - Bar.bsa", "Middle.bsa"] {
            std::fs::write(dir.join(n), b"").unwrap();
        }
        let order = Vfs::archive_order(&dir, &[], &["Foo.esp", "Middle.esp", "Foo - Bar.esp"]);
        assert_eq!(
            order,
            ["Foo - Main.bsa", "Foo.bsa", "Middle.bsa", "Foo - Bar.bsa"]
        );
        // Without the longer plugin, Foo.esp keeps claiming it.
        let order = Vfs::archive_order(&dir, &[], &["Foo.esp", "Middle.esp"]);
        assert_eq!(
            order,
            ["Foo - Bar.bsa", "Foo - Main.bsa", "Foo.bsa", "Middle.bsa"]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
