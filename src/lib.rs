use std::{
    fs::File,
    io::{Error, ErrorKind, Read, Result},
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
        Arc,
    },
    thread,
};

use rustix::process::{kill_process, Pid, Signal};
use tracing::{debug, error};

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

fn handle<F: FnMut(i32)>(mut master: File, mut callback: F) -> Result<()> {
    let mut last_progress = 0;
    loop {
        let mut data = [0; 0x1000];
        let count = master.read(&mut data)?;
        if count == 0 {
            return Ok(());
        }
        if let Ok(string) = str::from_utf8(&data[..count]) {
            for line in string.split(['\r', '\n']) {
                let len = line.len();
                if line.starts_with('[') && line.ends_with('%') && len >= 4 {
                    if let Ok(progress) = line[len - 4..len - 1].trim().parse::<i32>() {
                        if last_progress != progress {
                            callback(progress);
                            last_progress = progress;
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct Unsquashfs {
    cancel: Arc<AtomicBool>,
}

impl Default for Unsquashfs {
    fn default() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Unsquashfs {
    pub fn new() -> Self {
        Unsquashfs::default()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Extracts an image using either unsquashfs.
    /// ```rust,no_run
    ///     let unsquashfs = Unsquashfs::new();
    /// let unsquashfs_clone = unsquashfs.clone();
    /// thread::spawn(move || {
    ///     unsquashfs.extract(
    ///         "/home/saki/aosc-os_base_20240215_amd64.squashfs",
    ///         "/test",
    ///         None,
    ///         move |c| {
    ///             dbg!(c);
    ///         },
    ///     )
    ///     .unwrap();
    /// });
    // thread::sleep(Duration::from_secs(10));
    // unsquashfs_clone.cancel();
    /// ```
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

        command.arg("-f").arg("-d").arg(directory).arg(archive);

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
            forget(slave_stdin);
            forget(slave_stdout);
            forget(slave_stderr);
            child
        };

        let pid = Pid::from_child(&child);
        let cancel_success = Arc::new(AtomicBool::new(false));
        let extract_success = Arc::new(AtomicBool::new(false));
        let extract_success_clone = extract_success.clone();
        let cancel_success_clone = cancel_success.clone();
        let cancel_signal_clone = self.cancel.clone();

        thread::spawn(move || -> Result<()> {
            loop {
                if extract_success_clone.load(Ordering::SeqCst) {
                    return Ok(());
                }

                if cancel_signal_clone.load(Ordering::SeqCst) {
                    kill_process(pid, Signal::Term)?;
                    debug!("Canceled");
                    cancel_success_clone.store(true, Ordering::SeqCst);
                    cancel_signal_clone.store(false, Ordering::SeqCst);
                    return Ok(());
                }
                thread::sleep(std::time::Duration::from_millis(100));
            }
        });

        let master = unsafe { File::from_raw_fd(master_fd) };
        match handle(master, callback) {
            Ok(()) => (),
            Err(err) => match err.raw_os_error() {
                // EIO happens when slave end is closed
                Some(libc::EIO) => (),
                // Log other errors, use status code below to return
                _ => error!("handle error: {}", err),
            },
        }

        let cw = child.wait();
        let is_success = child.wait().map(|x| x.success()).unwrap_or(false);
        extract_success.store(is_success, Ordering::SeqCst);

        if is_success || cancel_success.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::Other,
                format!("archive extraction failed with status: {}", cw?),
            ))
        }
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
                .extract("testdata/test_extract.squashfs", &output, None, move |c| {
                    dbg!(c);
                })
                .unwrap();
            fs::remove_dir_all(output).unwrap();
        });

        thread::sleep(Duration::from_secs(1));
        unsquashfs_clone.cancel();
    }
}
