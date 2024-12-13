use std::{
    io::{self, BufRead, BufReader, Error, ErrorKind},
    path::Path,
    process::{ChildStdout, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    thread,
    time::Duration,
};

use pty_process::{
    blocking::{Command, Pty},
    Size,
};
use thiserror::Error;

fn handle(stdout: ChildStdout, mut callback: impl FnMut(i32)) -> io::Result<String> {
    let mut last_progress = 0;
    let mut output = String::new();
    let reader = BufReader::new(stdout);

    for line in reader.lines() {
        let line = line?;
        let len = line.len();
        if line.starts_with('[') && line.ends_with('%') && len >= 4 {
            if let Ok(progress) = line[len - 4..len - 1].trim().parse::<i32>() {
                if last_progress != progress {
                    callback(progress);
                    last_progress = progress;
                }
            }
        } else if len != 0 {
            output.push_str(&line);
            output.push('\n');
        }
    }

    Ok(output)
}

#[derive(Clone)]
pub struct Unsquashfs {
    cancel: Arc<AtomicBool>,
    status: Arc<RwLock<Status>>,
}

pub enum Status {
    Pending,
    Working,
}

impl Default for Unsquashfs {
    fn default() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            status: Arc::new(RwLock::new(Status::Pending)),
        }
    }
}

#[derive(Debug, Error)]
pub enum UnsquashfsError {
    #[error("`unsquashfs` binary does not exist.")]
    BinaryDoesNotExist,
    #[error(transparent)]
    IO(#[from] io::Error),
    #[error(transparent)]
    Pty(#[from] pty_process::Error),
    #[error("`unsquashfs` is not start.")]
    Pending,
    #[error("`unsquashfs` failed: {0}, output: {1}")]
    Failure(io::Error, String),
}

impl Unsquashfs {
    pub fn new() -> Self {
        Unsquashfs::default()
    }

    pub fn cancel(&self) -> Result<(), UnsquashfsError> {
        match *self.status.read().unwrap() {
            Status::Pending => Err(UnsquashfsError::Pending),
            Status::Working => {
                self.cancel.store(true, Ordering::SeqCst);
                Ok(())
            }
        }
    }

    /// Extracts an image using either unsquashfs.
    pub fn extract(
        &self,
        archive: impl AsRef<Path>,
        directory: impl AsRef<Path>,
        thread: Option<usize>,
        callback: impl FnMut(i32),
    ) -> Result<(), UnsquashfsError> {
        if which::which("unsquashfs").is_err() {
            return Err(UnsquashfsError::BinaryDoesNotExist);
        }

        let archive = archive.as_ref().canonicalize()?;
        let directory = directory.as_ref().canonicalize()?;

        let directory = directory
            .to_str()
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "Invalid directory path"))?
            .replace('\'', "'\"'\"'");

        let archive = archive
            .to_str()
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "Invalid archive path"))?
            .replace('\'', "'\"'\"'");

        let pty = Pty::new()?;
        pty.resize(Size::new(30, 80))?;

        let mut command = Command::new("unsquashfs");

        if let Some(limit_thread) = thread {
            command.arg("-p").arg(limit_thread.to_string());
        }

        command
            .arg("-f")
            .arg("-q")
            .arg("-d")
            .arg(directory)
            .arg(archive);

        let mut child = command
            .env("COLUMNS", "")
            .env("LINES", "")
            .env("TERM", "xterm-256color")
            .stdout(Stdio::piped())
            .spawn(&pty.pts()?)?;

        *self.status.write().unwrap() = Status::Working;

        let cc = self.cancel.clone();
        let status_clone = self.status.clone();

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::new(ErrorKind::BrokenPipe, "Failed to get stdout"))?;

        let process_control = thread::spawn(move || -> io::Result<()> {
            loop {
                let wait = child.try_wait()?;

                if cc.load(Ordering::SeqCst) {
                    child.kill()?;
                    cc.store(false, Ordering::SeqCst);
                    *status_clone.write().unwrap() = Status::Pending;
                    return Ok(());
                }

                let Some(wait) = wait else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };

                *status_clone.write().unwrap() = Status::Pending;

                if !wait.success() {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!(
                            "archive extraction failed with status: {}",
                            wait.code().unwrap_or(1),
                        ),
                    ));
                } else {
                    return Ok(());
                }
            }
        });

        let output = handle(stdout, callback)?;

        process_control
            .join()
            .unwrap()
            .map_err(|e| UnsquashfsError::Failure(e, output))?;

        Ok(())
    }
}

#[cfg(test)]
pub mod test {
    use std::{env::temp_dir, fs, thread, time::Duration};

    use crate::Unsquashfs;

    #[test]
    fn test_extract() {
        let unsquashfs = Unsquashfs::default();
        let unsquashfs_clone = unsquashfs.clone();

        let t = thread::spawn(move || {
            let output = temp_dir().join("unsqfs-wrap-test-extract");
            fs::create_dir_all(&output).unwrap();
            unsquashfs
                .extract(
                    "testdata/test_extract.squashfs",
                    &output,
                    None,
                    Box::new(move |c| {
                        dbg!(c);
                    }),
                )
                .unwrap();
            fs::remove_dir_all(output).unwrap();
        });

        thread::sleep(Duration::from_millis(10));
        unsquashfs_clone.cancel().unwrap();

        t.join().unwrap();
    }
}
