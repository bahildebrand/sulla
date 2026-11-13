use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use color_eyre::{Result, eyre::eyre};

pub struct RepoLock {
    _file: File,
}

pub fn acquire(repo: &Path) -> Result<RepoLock> {
    let dir = repo.join(".sulla");
    std::fs::create_dir_all(&dir)?;
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("lock"))?;
    match f.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let mut s = String::new();
            let _ = f.read_to_string(&mut s);
            let holder = match s.trim().parse::<u32>() {
                Ok(pid) => format!(" (pid {pid})"),
                Err(_) => String::new(),
            };
            return Err(eyre!(
                "sulla is already running in {}{holder}",
                repo.display()
            ));
        }
        Err(TryLockError::Error(e)) => return Err(e.into()),
    }
    f.set_len(0)?;
    f.seek(SeekFrom::Start(0))?;
    writeln!(f, "{}", std::process::id())?;
    f.flush()?;
    Ok(RepoLock { _file: f })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_fails_with_holder_pid_until_first_drops() {
        let dir = std::env::temp_dir().join(format!("sulla-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let first = acquire(&dir).unwrap();
        let err = acquire(&dir).err().unwrap().to_string();
        assert_eq!(
            err,
            format!(
                "sulla is already running in {} (pid {})",
                dir.display(),
                std::process::id()
            )
        );
        drop(first);
        let again = acquire(&dir);
        assert!(again.is_ok());
        drop(again);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
