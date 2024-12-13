use std::{
    fs::File,
    io::{self, Error, ErrorKind, Read, Result},
    mem::forget,
    os::unix::{
        io::{AsRawFd, FromRawFd, RawFd},
        process::CommandExt,
    },
    path::Path,
    process::{Command, Stdio},
    str,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    thread,
    time::Duration,
};

use tracing::debug;

fn getpty(columns: u32, lines: u32) -> (RawFd, String) {
    use std::{
        ffi::CStr,
        fs::OpenOptions,
        io,
        os::unix::{fs::OpenOptionsExt, io::IntoRawFd},
    };

    extern "C" {
        fn ptsname(fd: libc::c_int) -> *const libc::c_char;
        fn grantpt(fd: libc::c_int) -> libc::c_int;
        fn unlockpt(fd: libc::c_int) -> libc::c_int;
    }

    let master_fd = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open("/dev/ptmx")
        .unwrap()
        .into_raw_fd();
    unsafe {
        if grantpt(master_fd) < 0 {
            panic!("grantpt: {:?}", Error::last_os_error());
        }
        if unlockpt(master_fd) < 0 {
            panic!("unlockpt: {:?}", Error::last_os_error());
        }
    }

    unsafe {
        let size = libc::winsize {
            ws_row: lines as libc::c_ushort,
            ws_col: columns as libc::c_ushort,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if libc::ioctl(master_fd, libc::TIOCSWINSZ, &size as *const libc::winsize) < 0 {
            panic!("ioctl: {:?}", io::Error::last_os_error());
        }
    }

    let tty_path = unsafe {
        CStr::from_ptr(ptsname(master_fd))
            .to_string_lossy()
            .into_owned()
    };
    (master_fd, tty_path)
}

fn slave_stdio(tty_path: &str) -> Result<(File, File, File)> {
    use libc::{O_CLOEXEC, O_RDONLY, O_WRONLY};
    use std::ffi::CString;

    let cvt = |res: i32| -> Result<i32> {
        if res < 0 {
            Err(Error::last_os_error())
        } else {
            Ok(res)
        }
    };

    let tty_c = CString::new(tty_path).unwrap();
    let stdin =
        unsafe { File::from_raw_fd(cvt(libc::open(tty_c.as_ptr(), O_CLOEXEC | O_RDONLY))?) };
    let stdout =
        unsafe { File::from_raw_fd(cvt(libc::open(tty_c.as_ptr(), O_CLOEXEC | O_WRONLY))?) };
    let stderr =
        unsafe { File::from_raw_fd(cvt(libc::open(tty_c.as_ptr(), O_CLOEXEC | O_WRONLY))?) };

    Ok((stdin, stdout, stderr))
}

fn before_exec() -> Result<()> {
    unsafe {
        if libc::setsid() < 0 {
            panic!("setsid: {:?}", Error::last_os_error());
        }
        if libc::ioctl(0, libc::TIOCSCTTY, 1) < 0 {
            panic!("ioctl: {:?}", Error::last_os_error());
        }
    }

    Ok(())
}

fn handle(mut master: File, mut callback: impl FnMut(i32)) -> Result<String> {
    let mut last_progress = 0;
    let mut output = String::new();
    loop {
        let mut data = [0; 0x1000];
        let count = match master.read(&mut data) {
            Ok(0) => return Ok(output),
            Ok(c) => c,
            Err(err) => {
                return match err.raw_os_error() {
                    // EIO happens when slave end is closed
                    Some(libc::EIO) => Ok(output),
                    // Log other errors, use status code below to return
                    _ => Err(err),
                };
            }
        };

        if let Ok(string) = str::from_utf8(&data[..count]) {
            for line in string.split(['\r', '\n']) {
                let len = line.len();
                dbg!(line);
                if line.starts_with('[') && line.ends_with('%') && len >= 4 {
                    if let Ok(progress) = line[len - 4..len - 1].trim().parse::<i32>() {
                        if last_progress != progress {
                            callback(progress);
                            last_progress = progress;
                        }
                    }
                } else if len != 0 {
                    output.push_str(line);
                    output.push('\n');
                }
            }
        }
    }
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

impl Unsquashfs {
    pub fn new() -> Self {
        Unsquashfs::default()
    }

    pub fn cancel(&self) -> Result<()> {
        match *self.status.read().unwrap() {
            Status::Pending => Err(io::Error::new(ErrorKind::Other, "Unsquashfs is not start.")),
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
    ) -> Result<()> {
        if which::which("unsquashfs").is_err() {
            return Err(Error::new(
                ErrorKind::NotFound,
                "Unable to find unsquashfs binary.",
            ));
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

        debug!("{:?}", command);

        let (master_fd, tty_path) = getpty(80, 30);
        let mut child = {
            let (slave_stdin, slave_stdout, slave_stderr) = slave_stdio(&tty_path)?;

            let child = unsafe {
                command
                    .stdin(Stdio::from_raw_fd(slave_stdin.as_raw_fd()))
                    .stdout(Stdio::from_raw_fd(slave_stdout.as_raw_fd()))
                    .stderr(Stdio::from_raw_fd(slave_stderr.as_raw_fd()))
                    .env("COLUMNS", "")
                    .env("LINES", "")
                    .env("TERM", "xterm-256color")
                    .pre_exec(before_exec)
                    .spawn()?
            };

            *self.status.write().unwrap() = Status::Working;

            forget(slave_stdin);
            forget(slave_stdout);
            forget(slave_stderr);
            child
        };

        let cc = self.cancel.clone();
        let status_clone = self.status.clone();

        let process_control = thread::spawn(move || -> Result<()> {
            loop {
                let wait = child.try_wait()?;

                if cc.load(Ordering::SeqCst) {
                    child.kill()?;
                    cc.store(false, Ordering::SeqCst);
                    *status_clone.write().unwrap() = Status::Pending;
                    return Ok(());
                }

                dbg!(&wait);

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

        let master = unsafe { File::from_raw_fd(master_fd) };
        let output = handle(master, callback)?;

        process_control
            .join()
            .unwrap()
            .map_err(|e| io::Error::new(ErrorKind::Other, format!("Output: {}, {e}", output)))?;

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

        thread::spawn(move || {
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

        thread::sleep(Duration::from_secs(1));
        unsquashfs_clone.cancel().unwrap();
    }
}
