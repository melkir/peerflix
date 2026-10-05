//! Torrent storage that only creates the files a torrent writes to, and keeps
//! each as NAME.part until it's marked complete.
//!
//! librqbit's filesystem storage creates and opens every file of a torrent up
//! front, selected or not, which litters the download directory with empty
//! files and uses a file descriptor for each.

use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    io::IoSlice,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError},
};

use anyhow::{Context, bail};
use librqbit::{
    ManagedTorrentShared, TorrentMetadata,
    storage::{BoxStorageFactory, StorageFactory, StorageFactoryExt, TorrentStorage},
};

/// Both the factory librqbit creates the storage from and the storage itself,
/// sharing their state so peerflix can mark files complete.
#[derive(Clone)]
pub struct PartStorage {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    /// Set from the torrent's metadata when librqbit creates the storage.
    files: OnceLock<Vec<Slot>>,
}

struct Slot {
    /// Where the complete file goes.
    path: PathBuf,
    len: u64,
    open: Mutex<Option<Opened>>,
}

struct Opened {
    file: Arc<File>,
    /// Whether file is at path rather than at path.part.
    at_path: bool,
}

impl PartStorage {
    /// Stores the torrent's files under root, librqbit's output folder.
    pub fn new(root: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                root,
                files: OnceLock::new(),
            }),
        }
    }

    /// Renames file id from NAME.part to NAME, for once it's downloaded.
    pub fn complete(&self, id: usize) -> anyhow::Result<()> {
        let slot = self.slot(id)?;
        // The handle stays valid across the rename.
        if let Some(o) = slot.lock().as_mut().filter(|o| !o.at_path) {
            std::fs::rename(slot.part_path(), &slot.path)
                .with_context(|| format!("renaming {:?}", slot.part_path()))?;
            o.at_path = true;
        }
        Ok(())
    }

    fn slot(&self, id: usize) -> anyhow::Result<&Slot> {
        self.inner
            .files
            .get()
            .and_then(|f| f.get(id))
            .context("no such file")
    }
}

impl Slot {
    /// Locks the open file. A panic while it was locked leaves it consistent,
    /// since it's only ever replaced whole, so poisoning is ignored.
    fn lock(&self) -> MutexGuard<'_, Option<Opened>> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn part_path(&self) -> PathBuf {
        let mut p = OsString::from(self.path.as_os_str());
        p.push(".part");
        p.into()
    }

    /// Returns the file, opening it on first use: the file at path if it has
    /// the full length, which a finished or older download has, else the
    /// .part file, which is created only when create is set.
    fn file(&self, create: bool) -> anyhow::Result<Arc<File>> {
        let mut open = self.lock();
        if let Some(o) = open.as_ref() {
            return Ok(o.file.clone());
        }
        let at_path = std::fs::metadata(&self.path).is_ok_and(|m| m.len() == self.len);
        let path = if at_path {
            self.path.clone()
        } else {
            self.part_path()
        };
        if !at_path && !create && !path.exists() {
            bail!("{path:?} isn't downloaded");
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {dir:?}"))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {path:?}"))?;
        let file = Arc::new(file);
        *open = Some(Opened {
            file: file.clone(),
            at_path,
        });
        Ok(file)
    }
}

impl StorageFactory for PartStorage {
    type Storage = PartStorage;

    fn create(
        &self,
        _shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<PartStorage> {
        self.inner.files.get_or_init(|| {
            metadata
                .file_infos
                .iter()
                .map(|fi| Slot {
                    path: self.inner.root.join(&fi.relative_filename),
                    len: fi.len,
                    open: Mutex::new(None),
                })
                .collect()
        });
        Ok(self.clone())
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

impl TorrentStorage for PartStorage {
    fn init(
        &mut self,
        _shared: &ManagedTorrentShared,
        _metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        // Files are created on first write.
        Ok(())
    }

    // librqbit reads and writes nothing from the file before a chunk that
    // starts on a file boundary, which mustn't create that file or fail
    // because it isn't downloaded.
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let file = self.slot(file_id)?.file(false)?;
        Ok(file.read_exact_at(buf, offset)?)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let file = self.slot(file_id)?.file(true)?;
        Ok(file.write_all_at(buf, offset)?)
    }

    fn pwrite_all_vectored(
        &self,
        file_id: usize,
        offset: u64,
        bufs: [IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        if bufs.iter().all(|b| b.is_empty()) {
            return Ok(0);
        }
        let file = self.slot(file_id)?.file(true)?;
        let mut pos = offset;
        for b in &bufs {
            file.write_all_at(b, pos)?;
            pos += b.len() as u64;
        }
        Ok((pos - offset) as usize)
    }

    fn remove_file(&self, file_id: usize, _filename: &Path) -> anyhow::Result<()> {
        let slot = self.slot(file_id)?;
        *slot.lock() = None;
        for p in [slot.part_path(), slot.path.clone()] {
            match std::fs::remove_file(&p) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("removing {p:?}"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        let path = self.inner.root.join(path);
        if path.is_dir() && std::fs::read_dir(&path)?.next().is_none() {
            std::fs::remove_dir(&path).with_context(|| format!("removing {path:?}"))?;
        }
        Ok(())
    }

    /// Called for the selected files once the initial check is done, which
    /// is when they're created.
    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        Ok(self.slot(file_id)?.file(true)?.set_len(length)?)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(self.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage(dir: &Path, files: &[(&str, u64)]) -> PartStorage {
        let s = PartStorage::new(dir.to_owned());
        s.inner
            .files
            .set(
                files
                    .iter()
                    .map(|&(name, len)| Slot {
                        path: dir.join(name),
                        len,
                        open: Mutex::new(None),
                    })
                    .collect(),
            )
            .ok()
            .unwrap();
        s
    }

    #[test]
    fn writes_part_files_and_completes() {
        let dir = tempfile::tempdir().unwrap();
        let s = storage(dir.path(), &[("Show/E01.mkv", 10), ("Show/E02.mkv", 10)]);

        // Nothing exists until written, and reading a missing file fails.
        let mut buf = [0u8; 4];
        assert!(s.pread_exact(1, 0, &mut buf).is_err());
        // Empty reads and writes, at a chunk's file boundary, touch nothing.
        s.pread_exact(1, 10, &mut []).unwrap();
        s.pwrite_all(1, 10, &[]).unwrap();
        s.pwrite_all_vectored(1, 10, [IoSlice::new(&[]), IoSlice::new(&[])])
            .unwrap();
        assert!(!dir.path().join("Show").exists());

        s.ensure_file_length(0, 10).unwrap();
        s.pwrite_all(0, 2, b"abcd").unwrap();
        let part = dir.path().join("Show/E01.mkv.part");
        assert_eq!(std::fs::metadata(&part).unwrap().len(), 10);
        assert!(!dir.path().join("Show/E02.mkv.part").exists());
        s.pread_exact(0, 2, &mut buf).unwrap();
        assert_eq!(&buf, b"abcd");

        s.complete(0).unwrap();
        assert!(!part.exists());
        let done = dir.path().join("Show/E01.mkv");
        assert_eq!(&std::fs::read(&done).unwrap()[2..6], b"abcd");
        // Still readable and writable through the handle after the rename.
        s.pwrite_all(0, 0, b"zz").unwrap();
        s.pread_exact(0, 0, &mut buf[..2]).unwrap();
        assert_eq!(&buf[..2], b"zz");
        s.complete(0).unwrap();
    }

    #[test]
    fn reuses_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("done.mkv"), b"0123456789").unwrap();
        std::fs::write(dir.path().join("partial.mkv.part"), b"0123456789").unwrap();
        // A short file where the complete one goes is ignored.
        std::fs::write(dir.path().join("partial.mkv"), b"").unwrap();
        let s = storage(dir.path(), &[("done.mkv", 10), ("partial.mkv", 10)]);

        let mut buf = [0u8; 3];
        s.pread_exact(0, 1, &mut buf).unwrap();
        assert_eq!(&buf, b"123");
        s.pread_exact(1, 7, &mut buf).unwrap();
        assert_eq!(&buf, b"789");

        s.complete(1).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("partial.mkv")).unwrap(),
            b"0123456789"
        );
        assert!(!dir.path().join("partial.mkv.part").exists());
    }

    #[test]
    fn removes_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = storage(dir.path(), &[("sub/a.mkv", 4)]);
        s.pwrite_all(0, 0, b"abcd").unwrap();
        s.remove_file(0, Path::new("sub/a.mkv")).unwrap();
        assert!(!dir.path().join("sub/a.mkv.part").exists());
        s.remove_directory_if_empty(Path::new("sub")).unwrap();
        assert!(!dir.path().join("sub").exists());
    }
}
