use std::any::TypeId;
use std::collections::hash_map::{Entry, HashMap};
use std::ffi::{c_void, CString};
use std::fs::{File, Metadata};
use std::io::ErrorKind::NotFound;
#[cfg(unix)]
use std::os::unix::{
    ffi::OsStrExt,
    io::{AsRawFd, BorrowedFd, RawFd},
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
#[cfg(windows)]
use std::{
    ffi::OsStr,
    os::windows::io::{AsRawHandle, BorrowedHandle, RawHandle},
};
use std::{fmt, io, mem, ptr, sync};

use once_cell::sync::Lazy;
use synchronoise::event::SignalEvent;

use crate::mdb::error::mdb_result;
use crate::mdb::ffi;
use crate::{
    assert_eq_env_txn, Database, Error, Flag, PolyDatabase, Result, RoCursor, RoTxn, RwTxn,
};

/// The list of opened environments, the value is an optional environment, it is None
/// when someone asks to close the environment, closing is a two-phase step, to make sure
/// noone tries to open the same environment between these two phases.
///
/// Trying to open a None marked environment returns an error to the user trying to open it.
static OPENED_ENV: Lazy<RwLock<HashMap<PathBuf, EnvEntry>>> = Lazy::new(RwLock::default);

struct EnvEntry {
    env: Option<Env>,
    signal_event: Arc<SignalEvent>,
    options: EnvOpenOptions,
}

// Thanks to the mozilla/rkv project
// Workaround the UNC path on Windows, see https://github.com/rust-lang/rust/issues/42869.
// Otherwise, `Env::from_env()` will panic with error_no(123).
#[cfg(not(windows))]
fn canonicalize_path(path: &Path) -> io::Result<PathBuf> {
    path.canonicalize()
}

#[cfg(windows)]
fn canonicalize_path(path: &Path) -> io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    let url = url::Url::from_file_path(&canonical)
        .map_err(|_e| io::Error::new(io::ErrorKind::Other, "URL passing error"))?;
    url.to_file_path()
        .map_err(|_e| io::Error::new(io::ErrorKind::Other, "path canonicalization error"))
}

#[cfg(windows)]
/// Adding a 'missing' trait from windows OsStrExt
trait OsStrExtLmdb {
    fn as_bytes(&self) -> &[u8];
}
#[cfg(windows)]
impl OsStrExtLmdb for OsStr {
    fn as_bytes(&self) -> &[u8] {
        &self.to_str().unwrap().as_bytes()
    }
}

#[cfg(unix)]
fn get_file_fd(file: &File) -> RawFd {
    file.as_raw_fd()
}

#[cfg(windows)]
fn get_file_fd(file: &File) -> RawHandle {
    file.as_raw_handle()
}

#[cfg(unix)]
/// Get metadata from a file descriptor.
unsafe fn metadata_from_fd(raw_fd: RawFd) -> io::Result<Metadata> {
    let fd = BorrowedFd::borrow_raw(raw_fd);
    let owned = fd.try_clone_to_owned()?;
    File::from(owned).metadata()
}

#[cfg(windows)]
/// Get metadata from a file descriptor.
unsafe fn metadata_from_fd(raw_fd: RawHandle) -> io::Result<Metadata> {
    let fd = BorrowedHandle::borrow_raw(raw_fd);
    let owned = fd.try_clone_to_owned()?;
    File::from(owned).metadata()
}

/// Options and flags which can be used to configure how an environment is opened.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EnvOpenOptions {
    map_size: Option<usize>,
    max_readers: Option<u32>,
    max_dbs: Option<u32>,
    flags: u32, // LMDB flags
}

impl EnvOpenOptions {
    /// Creates a blank new set of options ready for configuration.
    pub fn new() -> EnvOpenOptions {
        EnvOpenOptions { map_size: None, max_readers: None, max_dbs: None, flags: 0 }
    }

    /// Set the size of the memory map to use for this environment.
    pub fn map_size(&mut self, size: usize) -> &mut Self {
        self.map_size = Some(size);
        self
    }

    /// Set the maximum number of threads/reader slots for the environment.
    pub fn max_readers(&mut self, readers: u32) -> &mut Self {
        self.max_readers = Some(readers);
        self
    }

    /// Set the maximum number of named databases for the environment.
    pub fn max_dbs(&mut self, dbs: u32) -> &mut Self {
        self.max_dbs = Some(dbs);
        self
    }

    /// Set one or [more LMDB flags](http://www.lmdb.tech/doc/group__mdb__env.html).
    /// ```
    /// use std::fs;
    /// use std::path::Path;
    /// use heed::{EnvOpenOptions, Database, Flag};
    /// use heed::types::*;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// fs::create_dir_all(Path::new("target").join("database.mdb"))?;
    /// let mut env_builder = EnvOpenOptions::new();
    /// unsafe {
    ///     env_builder.flag(Flag::NoTls);
    ///     env_builder.flag(Flag::NoMetaSync);
    /// }
    /// let dir = tempfile::tempdir().unwrap();
    /// let env = env_builder.open(dir.path())?;
    ///
    /// // we will open the default unamed database
    /// let mut wtxn = env.write_txn()?;
    /// let db: Database<Str, OwnedType<i32>> = env.create_database(&mut wtxn, None)?;
    ///
    /// // opening a write transaction
    /// db.put(&mut wtxn, "seven", &7)?;
    /// db.put(&mut wtxn, "zero", &0)?;
    /// db.put(&mut wtxn, "five", &5)?;
    /// db.put(&mut wtxn, "three", &3)?;
    /// wtxn.commit()?;
    ///
    /// // Force the OS to flush the buffers (see Flag::NoSync and Flag::NoMetaSync).
    /// env.force_sync();
    ///
    /// // opening a read transaction
    /// // to check if those values are now available
    /// let mut rtxn = env.read_txn()?;
    ///
    /// let ret = db.get(&rtxn, "zero")?;
    /// assert_eq!(ret, Some(0));
    ///
    /// let ret = db.get(&rtxn, "five")?;
    /// assert_eq!(ret, Some(5));
    /// # Ok(()) }
    /// ```
    pub unsafe fn flag(&mut self, flag: Flag) -> &mut Self {
        self.flags = self.flags | flag as u32;
        self
    }

    /// Open an environment that will be located at the specified path.
    pub fn open<P: AsRef<Path>>(&self, path: P) -> Result<Env> {
        let mut lock = OPENED_ENV.write().unwrap();

        let path = match canonicalize_path(path.as_ref()) {
            Err(err) => {
                if err.kind() == NotFound && self.flags & (Flag::NoSubDir as u32) != 0 {
                    let path = path.as_ref();
                    match path.parent().zip(path.file_name()) {
                        Some((dir, file_name)) => canonicalize_path(dir)?.join(file_name),
                        None => return Err(err.into()),
                    }
                } else {
                    return Err(err.into());
                }
            }
            Ok(path) => path,
        };

        match lock.entry(path) {
            Entry::Occupied(entry) => {
                let env = entry.get().env.clone().ok_or(Error::DatabaseClosing)?;
                let options = entry.get().options.clone();
                if &options == self {
                    return Ok(env);
                } else {
                    return Err(Error::BadOpenOptions { env, options });
                }
            }
            Entry::Vacant(entry) => {
                let path = entry.key();
                let path_str = CString::new(path.as_os_str().as_bytes()).unwrap();

                unsafe {
                    let mut env: *mut ffi::MDB_env = ptr::null_mut();
                    mdb_result(ffi::mdb_env_create(&mut env))?;

                    if let Some(size) = self.map_size {
                        if size % page_size::get() != 0 {
                            let msg = format!(
                                "map size ({}) must be a multiple of the system page size ({})",
                                size,
                                page_size::get()
                            );
                            return Err(Error::Io(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                msg,
                            )));
                        }
                        mdb_result(ffi::mdb_env_set_mapsize(env, size))?;
                    }

                    if let Some(readers) = self.max_readers {
                        mdb_result(ffi::mdb_env_set_maxreaders(env, readers))?;
                    }

                    if let Some(dbs) = self.max_dbs {
                        mdb_result(ffi::mdb_env_set_maxdbs(env, dbs))?;
                    }

                    // When the `sync-read-txn` feature is enabled, we must force LMDB
                    // to avoid using the thread local storage, this way we allow users
                    // to use references of RoTxn between threads safely.
                    let flags = if cfg!(feature = "sync-read-txn") {
                        self.flags | Flag::NoTls as u32
                    } else {
                        self.flags
                    };

                    let result =
                        mdb_result(ffi::mdb_env_open(env, path_str.as_ptr(), flags, 0o600));

                    match result {
                        Ok(()) => {
                            let signal_event = Arc::new(SignalEvent::manual(false));
                            let inner = EnvInner {
                                env,
                                dbi_open_mutex: sync::Mutex::default(),
                                path: path.clone(),
                            };
                            let env = Env(Arc::new(inner));
                            let cache_entry = EnvEntry {
                                env: Some(env.clone()),
                                options: self.clone(),
                                signal_event,
                            };
                            entry.insert(cache_entry);
                            Ok(env)
                        }
                        Err(e) => {
                            ffi::mdb_env_close(env);
                            Err(e.into())
                        }
                    }
                }
            }
        }
    }
}

/// Returns a struct that allows to wait for the effective closing of an environment.
pub fn env_closing_event<P: AsRef<Path>>(path: P) -> Option<EnvClosingEvent> {
    let lock = OPENED_ENV.read().unwrap();
    lock.get(path.as_ref()).map(|e| EnvClosingEvent(e.signal_event.clone()))
}

/// An environment handle constructed by using [`EnvOpenOptions`].
#[derive(Clone)]
pub struct Env(Arc<EnvInner>);

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let EnvInner { env: _, dbi_open_mutex: _, path } = self.0.as_ref();
        f.debug_struct("Env").field("path", &path.display()).finish_non_exhaustive()
    }
}

struct EnvInner {
    env: *mut ffi::MDB_env,
    dbi_open_mutex: sync::Mutex<HashMap<u32, DatabaseType>>,
    path: PathBuf,
}

unsafe impl Send for EnvInner {}

unsafe impl Sync for EnvInner {}

impl Drop for EnvInner {
    fn drop(&mut self) {
        let mut lock = OPENED_ENV.write().unwrap();

        match lock.remove(&self.path) {
            None => panic!("It seems another env closed this env before"),
            Some(EnvEntry { signal_event, .. }) => {
                unsafe {
                    let _ = ffi::mdb_env_close(self.env);
                }
                // We signal to all the waiters that the env is closed now.
                signal_event.signal();
            }
        }
    }
}

/// The type of the database.
pub enum DatabaseType {
    /// The first state of a database, unknown until an [`open_database`] method
    /// is called to define the type for the first time.
    UnknownYet,
    /// Defines the types of a [`Database`].
    Typed { key_type: TypeId, data_type: TypeId },
    /// Defines the types of a [`PolyDatabase`].
    Untyped,
}

/// Whether to perform compaction while copying an environment.
#[derive(Debug, Copy, Clone)]
pub enum CompactionOption {
    /// Omit free pages and sequentially renumber all pages in output.
    ///
    /// This option consumes more CPU and runs more slowly than the default.
    /// Currently it fails if the environment has suffered a page leak.
    Enabled,

    /// Copy everything without taking any special action about free pages.
    Disabled,
}

impl Env {
    pub(crate) fn env_mut_ptr(&self) -> *mut ffi::MDB_env {
        self.0.env
    }

    /// The size of the data file on disk.
    ///
    /// # Example
    ///
    /// ```
    /// use heed::EnvOpenOptions;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let dir = tempfile::tempdir()?;
    /// let size_in_bytes = 1024 * 1024;
    /// let env = EnvOpenOptions::new().map_size(size_in_bytes).open(dir.path())?;
    ///
    /// let actual_size = env.real_disk_size()? as usize;
    /// assert!(actual_size < size_in_bytes);
    /// # Ok(()) }
    /// ```
    pub fn real_disk_size(&self) -> Result<u64> {
        let mut fd = std::mem::MaybeUninit::uninit();
        unsafe { mdb_result(ffi::mdb_env_get_fd(self.env_mut_ptr(), fd.as_mut_ptr()))? };
        let fd = unsafe { fd.assume_init() };
        let metadata = unsafe { metadata_from_fd(fd)? };
        Ok(metadata.len())
    }

    /// Check if a flag was specified when opening this environment.
    pub fn contains_flag(&self, flag: Flag) -> Result<bool> {
        let flags = self.raw_flags()?;
        let set = flags & (flag as u32);
        Ok(set != 0)
    }

    /// Return the raw flags the environment was opened with.
    pub fn raw_flags(&self) -> Result<u32> {
        let mut flags = std::mem::MaybeUninit::uninit();
        unsafe { mdb_result(ffi::mdb_env_get_flags(self.env_mut_ptr(), flags.as_mut_ptr()))? };
        let flags = unsafe { flags.assume_init() };

        Ok(flags)
    }

    /// Returns some basic informations about this environment.
    pub fn info(&self) -> EnvInfo {
        let mut raw_info = mem::MaybeUninit::uninit();
        unsafe { ffi::mdb_env_info(self.0.env, raw_info.as_mut_ptr()) };
        let raw_info = unsafe { raw_info.assume_init() };

        EnvInfo {
            map_addr: raw_info.me_mapaddr,
            map_size: raw_info.me_mapsize,
            last_page_number: raw_info.me_last_pgno,
            last_txn_id: raw_info.me_last_txnid,
            maximum_number_of_readers: raw_info.me_maxreaders,
            number_of_readers: raw_info.me_numreaders,
        }
    }

    /// Returns the size used by all the databases in the environment without the free pages.
    pub fn non_free_pages_size(&self) -> Result<u64> {
        let compute_size = |stat: ffi::MDB_stat| {
            (stat.ms_leaf_pages + stat.ms_branch_pages + stat.ms_overflow_pages) as u64
                * stat.ms_psize as u64
        };

        let mut size = 0;

        let mut stat = std::mem::MaybeUninit::uninit();
        unsafe { mdb_result(ffi::mdb_env_stat(self.env_mut_ptr(), stat.as_mut_ptr()))? };
        let stat = unsafe { stat.assume_init() };
        size += compute_size(stat);

        let rtxn = self.read_txn()?;
        let dbi = self.raw_open_dbi(rtxn.txn, None, 0)?;

        // we don’t want anyone to open an environment while we’re computing the stats
        // thus we take a lock on the dbi
        let dbi_open = self.0.dbi_open_mutex.lock().unwrap();

        // We’re going to iterate on the unnamed database
        let mut cursor = RoCursor::new(&rtxn, dbi)?;

        while let Some((key, _value)) = cursor.move_on_next()? {
            if key.contains(&0) {
                continue;
            }

            let key = String::from_utf8(key.to_vec()).unwrap();
            if let Ok(dbi) = self.raw_open_dbi(rtxn.txn, Some(&key), 0) {
                let mut stat = std::mem::MaybeUninit::uninit();
                unsafe { mdb_result(ffi::mdb_stat(rtxn.txn, dbi, stat.as_mut_ptr()))? };
                let stat = unsafe { stat.assume_init() };
                size += compute_size(stat);

                // if the db wasn’t already opened
                if !dbi_open.contains_key(&dbi) {
                    unsafe { ffi::mdb_dbi_close(self.env_mut_ptr(), dbi) }
                }
            }
        }

        Ok(size)
    }

    /// Opens a typed database that already exists in this environment.
    ///
    /// If the database was previously opened in this program run, types will be checked.
    ///
    /// ## Important Information
    ///
    /// LMDB have an important restriction on the unnamed database when named ones are opened,
    /// the names of the named databases are stored as keys in the unnamed one and are immutable,
    /// these keys can only be read and not written.
    pub fn open_database<'t, KC, DC>(
        &self,
        rtxn: &'t RoTxn,
        name: Option<&str>,
    ) -> Result<Option<Database<'t, KC, DC>>>
    where
        KC: 'static,
        DC: 'static,
    {
        assert_eq_env_txn!(self, rtxn);

        let types = (TypeId::of::<KC>(), TypeId::of::<DC>());
        match self.raw_init_database(rtxn.txn, name, Some(types), false) {
            Ok(dbi) => Ok(Some(Database::new(self.env_mut_ptr() as _, dbi))),
            Err(Error::Mdb(e)) if e.not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Opens an untyped database that already exists in this environment.
    ///
    /// If the database was previously opened as a typed one, an error will be returned.
    ///
    /// ## Important Information
    ///
    /// LMDB have an important restriction on the unnamed database when named ones are opened,
    /// the names of the named databases are stored as keys in the unnamed one and are immutable,
    /// these keys can only be read and not written.
    pub fn open_poly_database<'t>(
        &self,
        rtxn: &'t RoTxn,
        name: Option<&str>,
    ) -> Result<Option<PolyDatabase<'t>>> {
        assert_eq_env_txn!(self, rtxn);

        match self.raw_init_database(rtxn.txn, name, None, false) {
            Ok(dbi) => Ok(Some(PolyDatabase::new(self.env_mut_ptr() as _, dbi))),
            Err(Error::Mdb(e)) if e.not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Creates a typed database that can already exist in this environment.
    ///
    /// If the database was previously opened in this program run, types will be checked.
    ///
    /// ## Important Information
    ///
    /// LMDB have an important restriction on the unnamed database when named ones are opened,
    /// the names of the named databases are stored as keys in the unnamed one and are immutable,
    /// these keys can only be read and not written.
    pub fn create_database<'t, KC, DC>(
        &self,
        wtxn: &'t mut RwTxn,
        name: Option<&str>,
    ) -> Result<Database<'t, KC, DC>>
    where
        KC: 'static,
        DC: 'static,
    {
        assert_eq_env_txn!(self, wtxn);

        let types = (TypeId::of::<KC>(), TypeId::of::<DC>());
        match self.raw_init_database(wtxn.txn.txn, name, Some(types), true) {
            Ok(dbi) => Ok(Database::new(self.env_mut_ptr() as _, dbi)),
            Err(e) => Err(e),
        }
    }

    /// Creates a typed database that can already exist in this environment.
    ///
    /// If the database was previously opened as a typed one, an error will be returned.
    ///
    /// ## Important Information
    ///
    /// LMDB have an important restriction on the unnamed database when named ones are opened,
    /// the names of the named databases are stored as keys in the unnamed one and are immutable,
    /// these keys can only be read and not written.
    pub fn create_poly_database<'t>(
        &self,
        wtxn: &'t mut RwTxn,
        name: Option<&str>,
    ) -> Result<PolyDatabase<'t>> {
        assert_eq_env_txn!(self, wtxn);

        match self.raw_init_database(wtxn.txn.txn, name, None, true) {
            Ok(dbi) => Ok(PolyDatabase::new(self.env_mut_ptr() as _, dbi)),
            Err(e) => Err(e),
        }
    }

    fn raw_open_dbi(
        &self,
        raw_txn: *mut ffi::MDB_txn,
        name: Option<&str>,
        flags: u32,
    ) -> std::result::Result<u32, crate::mdb::lmdb_error::Error> {
        let mut dbi = 0;
        let name = name.map(|n| CString::new(n).unwrap());
        let name_ptr = match name {
            Some(ref name) => name.as_bytes_with_nul().as_ptr() as *const _,
            None => ptr::null(),
        };

        // safety: The name cstring is cloned by LMDB, we can drop it after.
        //         If a read-only is used with the MDB_CREATE flag, LMDB will throw an error.
        unsafe { mdb_result(ffi::mdb_dbi_open(raw_txn, name_ptr, flags, &mut dbi))? };

        Ok(dbi)
    }

    fn raw_init_database(
        &self,
        raw_txn: *mut ffi::MDB_txn,
        name: Option<&str>,
        types: Option<(TypeId, TypeId)>,
        create: bool,
    ) -> Result<u32> {
        let mut lock = self.0.dbi_open_mutex.lock().unwrap();

        let flags = if create { ffi::MDB_CREATE } else { 0 };
        match self.raw_open_dbi(raw_txn, name, flags) {
            Ok(dbi) => {
                let types = match types {
                    Some((key_type, data_type)) => DatabaseType::Typed { key_type, data_type },
                    None => DatabaseType::Untyped,
                };

                let old_types = lock.entry(dbi).or_insert(types);
                if *old_types == types {
                    Ok(dbi)
                } else {
                    Err(Error::InvalidDatabaseTyping)
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Create a transaction with read and write access for use with the environment.
    pub fn write_txn(&self) -> Result<RwTxn> {
        RwTxn::new(self)
    }

    /// Create a nested transaction with read and write access for use with the environment.
    ///
    /// The new transaction will be a nested transaction, with the transaction indicated by parent
    /// as its parent. Transactions may be nested to any level.
    ///
    /// A parent transaction and its cursors may not issue any other operations than _commit_ and
    /// _abort_ while it has active child transactions.
    pub fn nested_write_txn<'p>(&'p self, parent: &'p mut RwTxn) -> Result<RwTxn<'p>> {
        RwTxn::nested(self, parent)
    }

    /// Create a transaction with read-only access for use with the environment.
    pub fn read_txn(&self) -> Result<RoTxn> {
        RoTxn::new(self)
    }

    /// Copy an LMDB environment to the specified path, with options.
    ///
    /// This function may be used to make a backup of an existing environment.
    /// No lockfile is created, since it gets recreated at need.
    pub fn copy_to_file<P: AsRef<Path>>(&self, path: P, option: CompactionOption) -> Result<File> {
        let file = File::options().create_new(true).write(true).open(&path)?;
        let fd = get_file_fd(&file);

        unsafe { self.copy_to_fd(fd, option)? };

        // We reopen the file to make sure the cursor is at the start,
        // even a seek to start doesn't work properly.
        let file = File::open(path)?;

        Ok(file)
    }

    /// Copy an LMDB environment to the specified file descriptor, with compaction option.
    ///
    /// This function may be used to make a backup of an existing environment.
    /// No lockfile is created, since it gets recreated at need.
    pub unsafe fn copy_to_fd(
        &self,
        fd: ffi::mdb_filehandle_t,
        option: CompactionOption,
    ) -> Result<()> {
        let flags = if let CompactionOption::Enabled = option { ffi::MDB_CP_COMPACT } else { 0 };
        mdb_result(ffi::mdb_env_copyfd2(self.0.env, fd, flags))?;
        Ok(())
    }

    /// Flush the data buffers to disk.
    pub fn force_sync(&self) -> Result<()> {
        unsafe { mdb_result(ffi::mdb_env_sync(self.0.env, 1))? }
        Ok(())
    }

    /// Returns the canonicalized path where this env lives.
    pub fn path(&self) -> &Path {
        &self.0.path
    }

    /// Returns an `EnvClosingEvent` that can be used to wait for the closing event,
    /// multiple threads can wait on this event.
    ///
    /// Make sure that you drop all the copies of `Env`s you have, env closing are triggered
    /// when all references are dropped, the last one will eventually close the environment.
    pub fn prepare_for_closing(self) -> EnvClosingEvent {
        let mut lock = OPENED_ENV.write().unwrap();
        match lock.get_mut(self.path()) {
            None => panic!("cannot find the env that we are trying to close"),
            Some(EnvEntry { env, signal_event, .. }) => {
                // We remove the env from the global list and replace it with a None.
                let _env = env.take();
                let signal_event = signal_event.clone();

                // we must make sure we release the lock before we drop the env
                // as the drop of the EnvInner also tries to lock the OPENED_ENV
                // global and we don't want to trigger a dead-lock.
                drop(lock);

                EnvClosingEvent(signal_event)
            }
        }
    }

    /// Check for stale entries in the reader lock table and clear them.
    ///
    /// Returns the number of stale readers cleared.
    pub fn clear_stale_readers(&self) -> Result<usize> {
        let mut dead: i32 = 0;
        unsafe { mdb_result(ffi::mdb_reader_check(self.0.env, &mut dead))? }
        // safety: The reader_check function asks for an i32, initialize it to zero
        //         and never decrements it. It is safe to use either an u32 or u64 (usize).
        Ok(dead as usize)
    }
}

/// Contains information about the environment.
#[derive(Debug, Clone, Copy)]
pub struct EnvInfo {
    /// Address of map, if fixed.
    pub map_addr: *mut c_void,
    /// Size of the data memory map.
    pub map_size: usize,
    /// ID of the last used page.
    pub last_page_number: usize,
    /// ID of the last committed transaction.
    pub last_txn_id: usize,
    /// Maximum number of reader slots in the environment.
    pub maximum_number_of_readers: u32,
    /// Maximum number of reader slots used in the environment.
    pub number_of_readers: u32,
}

/// A structure that can be used to wait for the closing event,
/// multiple threads can wait on this event.
#[derive(Clone)]
pub struct EnvClosingEvent(Arc<SignalEvent>);

impl EnvClosingEvent {
    /// Blocks this thread until the environment is effectively closed.
    ///
    /// # Safety
    ///
    /// Make sure that you don't have any copy of the environment in the thread
    /// that is waiting for a close event, if you do, you will have a dead-lock.
    pub fn wait(&self) {
        self.0.wait()
    }

    /// Blocks this thread until either the environment has been closed
    /// or until the timeout elapses, returns `true` if the environment
    /// has been effectively closed.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        self.0.wait_timeout(timeout)
    }
}

impl fmt::Debug for EnvClosingEvent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("EnvClosingEvent").finish()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::{fs, thread};

    use crate::types::*;
    use crate::{env_closing_event, EnvOpenOptions, Error};

    #[test]
    fn close_env() {
        let dir = tempfile::tempdir().unwrap();
        let env = EnvOpenOptions::new()
            .map_size(10 * 1024 * 1024) // 10MB
            .max_dbs(30)
            .open(&dir.path())
            .unwrap();

        // Force a thread to keep the env for 1 second.
        let env_cloned = env.clone();
        thread::spawn(move || {
            let _env = env_cloned;
            thread::sleep(Duration::from_secs(1));
        });

        let mut wtxn = env.write_txn().unwrap();
        let db = env.create_database::<Str, Str>(&mut wtxn, None).unwrap();
        wtxn.commit().unwrap();

        // Create an ordered list of keys...
        let mut wtxn = env.write_txn().unwrap();
        db.put(&mut wtxn, "hello", "hello").unwrap();
        db.put(&mut wtxn, "world", "world").unwrap();

        let mut iter = db.iter(&wtxn).unwrap();
        assert_eq!(iter.next().transpose().unwrap(), Some(("hello", "hello")));
        assert_eq!(iter.next().transpose().unwrap(), Some(("world", "world")));
        assert_eq!(iter.next().transpose().unwrap(), None);
        drop(iter);

        wtxn.commit().unwrap();

        let signal_event = env.prepare_for_closing();

        eprintln!("waiting for the env to be closed");
        signal_event.wait();
        eprintln!("env closed successfully");

        // Make sure we don't have a reference to the env
        assert!(env_closing_event(&dir.path()).is_none());
    }

    #[test]
    fn reopen_env_with_different_options_is_err() {
        let dir = tempfile::tempdir().unwrap();
        let _env = EnvOpenOptions::new()
            .map_size(10 * 1024 * 1024) // 10MB
            .open(&dir.path())
            .unwrap();

        let result = EnvOpenOptions::new()
            .map_size(12 * 1024 * 1024) // 12MB
            .open(&dir.path());

        assert!(matches!(result, Err(Error::BadOpenOptions { .. })));
    }

    #[test]
    fn open_env_with_named_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("babar.mdb")).unwrap();
        let _env = EnvOpenOptions::new()
            .map_size(10 * 1024 * 1024) // 10MB
            .open(dir.path().join("babar.mdb"))
            .unwrap();

        let _env = EnvOpenOptions::new()
            .map_size(10 * 1024 * 1024) // 10MB
            .open(dir.path().join("babar.mdb"))
            .unwrap();
    }

    #[test]
    #[cfg(not(windows))]
    fn open_database_with_writemap_flag() {
        let dir = tempfile::tempdir().unwrap();
        let mut envbuilder = EnvOpenOptions::new();
        envbuilder.map_size(10 * 1024 * 1024); // 10MB
        envbuilder.max_dbs(10);
        unsafe { envbuilder.flag(crate::Flag::WriteMap) };
        let env = envbuilder.open(&dir.path()).unwrap();

        let mut wtxn = env.write_txn().unwrap();
        let _db = env.create_database::<Str, Str>(&mut wtxn, Some("my-super-db")).unwrap();
        wtxn.commit().unwrap();
    }

    #[test]
    fn open_database_with_nosubdir() {
        let dir = tempfile::tempdir().unwrap();
        let mut envbuilder = EnvOpenOptions::new();
        unsafe { envbuilder.flag(crate::Flag::NoSubDir) };
        let _env = envbuilder.open(&dir.path().join("data.mdb")).unwrap();
    }

    #[test]
    fn create_database_without_commit() {
        let dir = tempfile::tempdir().unwrap();
        let env = EnvOpenOptions::new()
            .map_size(10 * 1024 * 1024) // 10MB
            .max_dbs(10)
            .open(&dir.path())
            .unwrap();

        let mut wtxn = env.write_txn().unwrap();
        let _db = env.create_database::<Str, Str>(&mut wtxn, Some("my-super-db")).unwrap();
        wtxn.abort();

        let rtxn = env.read_txn().unwrap();
        let option = env.open_database::<Str, Str>(&rtxn, Some("my-super-db")).unwrap();
        assert!(option.is_none());
    }

    #[test]
    fn open_already_existing_database() {
        let dir = tempfile::tempdir().unwrap();
        let env = EnvOpenOptions::new()
            .map_size(10 * 1024 * 1024) // 10MB
            .max_dbs(10)
            .open(&dir.path())
            .unwrap();

        // we first create a database
        let mut wtxn = env.write_txn().unwrap();
        let _db = env.create_database::<Str, Str>(&mut wtxn, Some("my-super-db")).unwrap();
        wtxn.commit().unwrap();

        // Close the environement and reopen it, databases must not be loaded in memory.
        env.prepare_for_closing().wait();
        let env = EnvOpenOptions::new()
            .map_size(10 * 1024 * 1024) // 10MB
            .max_dbs(10)
            .open(&dir.path())
            .unwrap();

        let rtxn = env.read_txn().unwrap();
        let option = env.open_database::<Str, Str>(&rtxn, Some("my-super-db")).unwrap();
        assert!(option.is_some());
    }
}
