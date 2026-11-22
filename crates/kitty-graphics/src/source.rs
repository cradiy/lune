use crate::{
    Controls, Value,
    pixels::{INVALID, UNSUPPORTED},
};

pub(crate) const BAD_SOURCE: &[u8] = b"EBADF:Failed to read image file";

#[derive(Clone, Copy)]
pub(crate) enum Medium {
    File,
    Temporary,
    Shared,
}

pub(crate) struct LocalSource {
    medium: Medium,
    offset: u64,
    size: Option<usize>,
}

impl LocalSource {
    pub(crate) fn parse(controls: &Controls) -> Result<Option<Self>, &'static [u8]> {
        let medium = match controls.get(b't') {
            None | Some(Value::Letter(b'd')) => return Ok(None),
            Some(Value::Letter(b'f')) => Medium::File,
            Some(Value::Letter(b't')) => Medium::Temporary,
            Some(Value::Letter(b's')) => Medium::Shared,
            _ => return Err(UNSUPPORTED),
        };
        if !matches!(controls.get(b'm'), None | Some(Value::Unsigned(0))) {
            return Err(INVALID);
        }
        let optional = |key| match controls.get(key) {
            None => Ok(0),
            Some(Value::Unsigned(value)) => Ok(value),
            _ => Err(INVALID),
        };
        let size = optional(b'S')? as usize;
        Ok(Some(Self {
            medium,
            offset: u64::from(optional(b'O')?),
            size: (size != 0).then_some(size),
        }))
    }

    pub(crate) fn read(
        self,
        name: &[u8],
        limit: usize,
        fixed_capacity: Option<usize>,
    ) -> Result<Vec<u8>, &'static [u8]> {
        read(self, name, limit, fixed_capacity)
    }
}

#[cfg(target_os = "linux")]
fn read(
    source: LocalSource,
    name: &[u8],
    limit: usize,
    fixed_capacity: Option<usize>,
) -> Result<Vec<u8>, &'static [u8]> {
    linux::read(source, name, limit, fixed_capacity).ok_or(BAD_SOURCE)
}

#[cfg(not(target_os = "linux"))]
fn read(
    source: LocalSource,
    _: &[u8],
    _: usize,
    _: Option<usize>,
) -> Result<Vec<u8>, &'static [u8]> {
    let LocalSource {
        medium: _,
        offset: _,
        size: _,
    } = source;
    Err(UNSUPPORTED)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        ffi::{OsStr, OsString},
        fs::File,
        io::{Read, Seek, SeekFrom},
        os::{
            fd::AsRawFd,
            unix::{
                ffi::{OsStrExt, OsStringExt},
                fs::FileExt,
            },
        },
        path::{Path, PathBuf},
    };

    use rustix::{
        fd::OwnedFd,
        fs::{self, AtFlags, FileType, Mode, OFlags, ResolveFlags, Stat},
        process, shm,
    };

    use super::{LocalSource, Medium};

    const SHARED_DIRECTORY: &str = "/dev/shm";
    const TEMP_MARKER: &[u8] = b"tty-graphics-protocol";
    const SMALL_READ_BYTES: usize = 4096;

    pub(super) fn read(
        source: LocalSource,
        name: &[u8],
        limit: usize,
        fixed_capacity: Option<usize>,
    ) -> Option<Vec<u8>> {
        let name = OsStr::from_bytes(name);
        if name.is_empty() || name.as_bytes().contains(&0) {
            return None;
        }
        match source.medium {
            Medium::File | Medium::Temporary => {
                let path = Path::new(name);
                let fd = fs::openat2(
                    fs::CWD,
                    path,
                    OFlags::PATH | OFlags::CLOEXEC,
                    Mode::empty(),
                    ResolveFlags::NO_MAGICLINKS,
                )
                .ok()?;
                let (file, stat, resolved) = readable(&fd)?;
                let data = read_range(file, &stat, &source, limit, fixed_capacity);
                if matches!(source.medium, Medium::Temporary) {
                    unlink_temporary(&resolved, &stat);
                }
                data
            }
            Medium::Shared => {
                let entry = name
                    .as_bytes()
                    .strip_prefix(b"/")
                    .unwrap_or(name.as_bytes());
                if entry.is_empty()
                    || entry.len() > 255
                    || entry == b"."
                    || entry == b".."
                    || entry.contains(&b'/')
                {
                    return None;
                }
                let entry = OsStr::from_bytes(entry);
                let dir = fs::open(
                    SHARED_DIRECTORY,
                    OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .ok()?;
                let fd = fs::openat2(
                    &dir,
                    entry,
                    OFlags::PATH | OFlags::CLOEXEC,
                    Mode::empty(),
                    ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
                )
                .ok()?;
                let (file, stat, _) = readable(&fd)?;
                let data = read_range(file, &stat, &source, limit, fixed_capacity);
                if same_entry(&dir, entry, &stat) {
                    let _ = shm::unlink(name);
                }
                data
            }
        }
    }

    fn readable(fd: &OwnedFd) -> Option<(File, Stat, PathBuf)> {
        let stat = fs::fstat(fd).ok()?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
            || fs::fstatfs(fd).ok()?.f_type == fs::PROC_SUPER_MAGIC
        {
            return None;
        }
        let descriptor_path = format!("/proc/self/fd/{}", fd.as_raw_fd());
        let link = fs::readlink(descriptor_path.as_str(), Vec::new()).ok()?;
        let resolved = PathBuf::from(OsString::from_vec(link.into_bytes()));
        if !resolved.is_absolute()
            || resolved.starts_with("/proc")
            || resolved.starts_with("/sys")
            || (resolved.starts_with("/dev") && !resolved.starts_with(SHARED_DIRECTORY))
        {
            return None;
        }
        // Reopen the checked inode by its descriptor, keeping the O_PATH fd live.
        let file = fs::open(
            descriptor_path.as_str(),
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .ok()?;
        Some((file.into(), stat, resolved))
    }

    fn read_range(
        mut file: File,
        stat: &Stat,
        source: &LocalSource,
        limit: usize,
        fixed_capacity: Option<usize>,
    ) -> Option<Vec<u8>> {
        let available = u64::try_from(stat.st_size)
            .ok()?
            .checked_sub(source.offset)?;
        let requested = source.size.map_or(available, |size| size as u64);
        // A fixed-size transfer only needs the bytes covered by its pixel dimensions.
        let len = fixed_capacity.map_or(requested, |_| limit as u64);
        if len > requested.min(available) || len > limit as u64 {
            return None;
        }
        let len = len as usize;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(fixed_capacity.unwrap_or(len))
            .ok()?;
        if len <= SMALL_READ_BYTES {
            bytes.resize(len, 0);
            file.read_exact_at(&mut bytes, source.offset).ok()?;
            return Some(bytes);
        }
        if source.offset != 0 {
            file.seek(SeekFrom::Start(source.offset)).ok()?;
        }
        let read = file.take(len as u64).read_to_end(&mut bytes).ok()?;
        (read == len).then_some(bytes)
    }

    fn same_entry(dir: &OwnedFd, entry: &OsStr, expected: &Stat) -> bool {
        fs::statat(dir, entry, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|actual| {
            actual.st_dev == expected.st_dev && actual.st_ino == expected.st_ino
        })
    }

    fn unlink_temporary(path: &Path, stat: &Stat) {
        if stat.st_uid != process::geteuid().as_raw()
            || !path
                .as_os_str()
                .as_bytes()
                .windows(TEMP_MARKER.len())
                .any(|part| part == TEMP_MARKER)
        {
            return;
        }
        let Some((parent, name)) = path.parent().zip(path.file_name()) else {
            return;
        };
        let dirs = [
            PathBuf::from("/tmp"),
            PathBuf::from(SHARED_DIRECTORY),
            std::env::temp_dir(),
        ];
        for root in dirs {
            let Ok(root) = root.canonicalize() else {
                continue;
            };
            let Ok(relative) = parent.strip_prefix(root.as_path()) else {
                continue;
            };
            let Ok(root_fd) = fs::open(
                &root,
                OFlags::PATH | OFlags::CLOEXEC | OFlags::DIRECTORY,
                Mode::empty(),
            ) else {
                continue;
            };
            let dir = if relative.as_os_str().is_empty() {
                root_fd
            } else {
                let Ok(dir) = fs::openat2(
                    &root_fd,
                    relative,
                    OFlags::PATH | OFlags::CLOEXEC | OFlags::DIRECTORY,
                    Mode::empty(),
                    ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
                ) else {
                    continue;
                };
                dir
            };
            if same_entry(&dir, name, stat) {
                let _ = fs::unlinkat(&dir, name, AtFlags::empty());
            }
            return;
        }
    }
}
