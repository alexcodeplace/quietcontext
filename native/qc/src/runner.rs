use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const FILTER_SAMPLE_CAP_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_RAW_SPOOL_CAP_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct CapturedStream {
    pub sample: Vec<u8>,
    pub sample_complete: bool,
    pub raw_path: PathBuf,
    pub raw_bytes: usize,
    pub raw_complete: bool,
}

#[derive(Debug)]
pub struct RunResult {
    pub stdout: CapturedStream,
    pub stderr: CapturedStream,
    pub exit_code: i32,
}

fn raw_spool_cap() -> usize {
    std::env::var("QUIET_CONTEXT_NATIVE_RAW_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v >= 1024)
        .unwrap_or(DEFAULT_RAW_SPOOL_CAP_BYTES)
}

fn spool_dir() -> std::io::Result<PathBuf> {
    let dir = if let Ok(path) = std::env::var("QUIET_CONTEXT_NATIVE_SPOOL_DIR") {
        if path.trim().is_empty() {
            crate::util::state_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("spool")
        } else {
            PathBuf::from(path)
        }
    } else {
        crate::util::state_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("spool")
    };
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

fn create_spool_file(dir: &std::path::Path, stream: &str) -> std::io::Result<(PathBuf, File)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for nonce in 0_u32..1000 {
        let path = dir.join(format!("run-{}-{now}-{nonce}.{stream}", std::process::id()));
        let mut opts = OpenOptions::new();
        opts.create_new(true).write(true).read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate unique native spool file",
    ))
}

fn capture_stream<R: Read + Send + 'static>(
    mut reader: R,
    stream: &'static str,
    spool_dir: PathBuf,
    raw_cap: usize,
) -> impl FnOnce() -> CapturedStream + Send + 'static {
    move || {
        let (raw_path, mut raw_file) = match create_spool_file(&spool_dir, stream) {
            Ok(v) => v,
            Err(_) => {
                let fallback = std::env::temp_dir().join(format!(
                    "qc-native-unavailable-{}-{stream}",
                    std::process::id()
                ));
                let file = OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(&fallback)
                    .expect("temporary spool fallback must be writable");
                (fallback, file)
            }
        };
        let mut sample = Vec::with_capacity(FILTER_SAMPLE_CAP_BYTES.min(256 * 1024));
        let mut buf = [0_u8; 16 * 1024];
        let mut raw_bytes = 0usize;
        let mut raw_written = 0usize;
        let mut raw_complete = true;
        let mut sample_complete = true;

        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => {
                    raw_complete = false;
                    break;
                }
            };
            raw_bytes = raw_bytes.saturating_add(n);

            if sample.len() < FILTER_SAMPLE_CAP_BYTES {
                let remaining = FILTER_SAMPLE_CAP_BYTES - sample.len();
                let take = remaining.min(n);
                sample.extend_from_slice(&buf[..take]);
                if take < n {
                    sample_complete = false;
                }
            } else {
                sample_complete = false;
            }

            if raw_written < raw_cap {
                let remaining = raw_cap - raw_written;
                let take = remaining.min(n);
                if take > 0 {
                    if raw_file.write_all(&buf[..take]).is_err() {
                        raw_complete = false;
                    } else {
                        raw_written += take;
                    }
                }
                if take < n {
                    raw_complete = false;
                }
            } else {
                raw_complete = false;
            }
        }
        let _ = raw_file.flush();
        if raw_written != raw_bytes {
            raw_complete = false;
        }

        CapturedStream {
            sample,
            sample_complete,
            raw_path,
            raw_bytes,
            raw_complete,
        }
    }
}

#[derive(Clone, Debug)]
struct CapturePolicy {
    spool_dir: PathBuf,
    raw_cap: usize,
}

impl CapturePolicy {
    fn from_env() -> Self {
        Self {
            spool_dir: spool_dir().unwrap_or_else(|_| std::env::temp_dir()),
            raw_cap: raw_spool_cap(),
        }
    }
}

/// Execute one command without killing it merely because output is large.
/// Exact raw bytes are spooled up to a bounded per-stream cap while a bounded
/// sample feeds the structural filter. The caller decides whether to archive
/// or remove the spool files.
pub fn run_command(binary: &OsStr, args: &[OsString]) -> RunResult {
    run_command_with_policy(binary, args, CapturePolicy::from_env())
}

fn run_command_with_policy(binary: &OsStr, args: &[OsString], policy: CapturePolicy) -> RunResult {
    let mut child = match Command::new(binary)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            let (stdout_path, mut out) =
                create_spool_file(&policy.spool_dir, "stdout").expect("spool");
            let (err_path, mut err) =
                create_spool_file(&policy.spool_dir, "stderr").expect("spool");
            let msg = format!("qc: command not found: {}\n", binary.to_string_lossy());
            let _ = out.flush();
            let _ = err.write_all(msg.as_bytes());
            let _ = err.flush();
            return RunResult {
                stdout: CapturedStream {
                    sample: Vec::new(),
                    sample_complete: true,
                    raw_path: stdout_path,
                    raw_bytes: 0,
                    raw_complete: true,
                },
                stderr: CapturedStream {
                    sample: msg.as_bytes().to_vec(),
                    sample_complete: true,
                    raw_path: err_path,
                    raw_bytes: msg.len(),
                    raw_complete: true,
                },
                exit_code: 127,
            };
        }
    };

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_handle = thread::spawn(capture_stream(
        stdout,
        "stdout",
        policy.spool_dir.clone(),
        policy.raw_cap,
    ));
    let stderr_handle = thread::spawn(capture_stream(
        stderr,
        "stderr",
        policy.spool_dir.clone(),
        policy.raw_cap,
    ));
    let status = child.wait();
    let stdout = stdout_handle
        .join()
        .unwrap_or_else(|_| panic!("stdout capture thread panicked"));
    let stderr = stderr_handle
        .join()
        .unwrap_or_else(|_| panic!("stderr capture thread panicked"));

    let exit_code = match status {
        Ok(status) => status.code().unwrap_or_else(|| {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                status.signal().map(|s| 128 + s).unwrap_or(1)
            }
            #[cfg(not(unix))]
            {
                1
            }
        }),
        Err(_) => 1,
    };
    RunResult {
        stdout,
        stderr,
        exit_code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn preserves_exit_code_and_raw_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let args = vec![
            OsString::from("-c"),
            OsString::from("printf hello; printf err >&2; exit 7"),
        ];
        let result = run_command_with_policy(
            OsStr::new("sh"),
            &args,
            CapturePolicy {
                spool_dir: tmp.path().to_path_buf(),
                raw_cap: DEFAULT_RAW_SPOOL_CAP_BYTES,
            },
        );
        assert_eq!(result.exit_code, 7);
        assert_eq!(fs::read(&result.stdout.raw_path).unwrap(), b"hello");
        assert_eq!(fs::read(&result.stderr.raw_path).unwrap(), b"err");
        assert!(result.stdout.raw_complete);
        assert!(result.stderr.raw_complete);
    }

    #[test]
    fn large_output_is_not_killed_or_reported_success_artificially() {
        let tmp = tempfile::tempdir().unwrap();
        let args = vec![
            OsString::from("-c"),
            OsString::from("head -c 10000 /dev/zero; exit 23"),
        ];
        let result = run_command_with_policy(
            OsStr::new("sh"),
            &args,
            CapturePolicy {
                spool_dir: tmp.path().to_path_buf(),
                raw_cap: 4096,
            },
        );
        assert_eq!(result.exit_code, 23);
        assert_eq!(result.stdout.raw_bytes, 10000);
        assert!(!result.stdout.raw_complete);
        assert_eq!(fs::metadata(&result.stdout.raw_path).unwrap().len(), 4096);
    }
}
