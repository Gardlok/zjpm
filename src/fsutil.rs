use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let (temporary_path, mut temporary_file) = create_temporary_file(parent, path)?;

    let result = (|| {
        temporary_file.write_all(contents)?;
        temporary_file.flush()?;
        temporary_file.sync_all()?;
        drop(temporary_file);

        fs::rename(&temporary_path, path)?;
        sync_directory(parent)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }

    result
}

fn create_temporary_file(parent: &Path, target: &Path) -> io::Result<(PathBuf, File)> {
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("zjpm");

    for _ in 0..1024 {
        let sequence = WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{name}.{}-{sequence}.part", std::process::id()));

        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary file",
    ))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn atomic_write_replaces_existing_contents() {
        let root = env::temp_dir().join(format!("zjpm-write-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("state.kdl");

        write_atomic(&path, b"first\n").unwrap();
        write_atomic(&path, b"second\n").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"second\n");
        assert_eq!(
            fs::read_dir(&root).unwrap().filter_map(Result::ok).count(),
            1
        );

        fs::remove_dir_all(root).unwrap();
    }
}
