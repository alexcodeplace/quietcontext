use crate::config::MapConfig;
use crate::repomap;
use crate::repomap_protocol::{
    self, CacheState, LookupRequest, LookupResponse, LookupTimings, PROTOCOL_VERSION,
};
use std::io;
#[cfg(unix)]
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) const DAEMON_MODE_ARG: &str = "--repomap-daemon";
pub(crate) const DAEMON_MODE_ENV: &str = "QUIET_CONTEXT_REPOMAP_DAEMON";
pub(crate) const SOCKET_REVISION: &str = "v2";

const STARTUP_LOCK_WAIT: Duration = Duration::from_millis(5);
const EXISTING_CONNECT_WAIT: Duration = Duration::from_millis(2);
const LAZY_START_WAIT: Duration = Duration::from_millis(100);
const REQUEST_WAIT: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClientBudget {
    pub(crate) startup_lock_wait: Duration,
    pub(crate) existing_connect_wait: Duration,
    pub(crate) startup_wait: Duration,
    pub(crate) request_wait: Duration,
}

impl ClientBudget {
    #[cfg(test)]
    pub(crate) const fn new(
        startup_lock_wait: Duration,
        existing_connect_wait: Duration,
        startup_wait: Duration,
        request_wait: Duration,
    ) -> Self {
        Self {
            startup_lock_wait,
            existing_connect_wait,
            startup_wait,
            request_wait,
        }
    }

    fn bounded(self) -> Self {
        Self {
            startup_lock_wait: self.startup_lock_wait.min(STARTUP_LOCK_WAIT),
            existing_connect_wait: self.existing_connect_wait.min(EXISTING_CONNECT_WAIT),
            startup_wait: self.startup_wait.min(LAZY_START_WAIT),
            request_wait: self.request_wait.min(REQUEST_WAIT),
        }
    }
}

impl Default for ClientBudget {
    fn default() -> Self {
        Self {
            startup_lock_wait: STARTUP_LOCK_WAIT,
            existing_connect_wait: EXISTING_CONNECT_WAIT,
            startup_wait: LAZY_START_WAIT,
            request_wait: REQUEST_WAIT,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FallbackReason {
    InvalidRequest,
    DaemonMode,
    #[cfg(not(unix))]
    UnsupportedPlatform,
    RuntimeUnavailable,
    RuntimeUnsafe,
    PeerRejected,
    StartupBusy,
    SpawnFailed,
    StartupTimeout,
    RequestTimeout,
    ProtocolMismatch,
    DaemonUnavailable,
}

impl FallbackReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid-request",
            Self::DaemonMode => "daemon-mode",
            #[cfg(not(unix))]
            Self::UnsupportedPlatform => "unsupported-platform",
            Self::RuntimeUnavailable => "runtime-unavailable",
            Self::RuntimeUnsafe => "runtime-unsafe",
            Self::PeerRejected => "peer-rejected",
            Self::StartupBusy => "startup-busy",
            Self::SpawnFailed => "spawn-failed",
            Self::StartupTimeout => "startup-timeout",
            Self::RequestTimeout => "request-timeout",
            Self::ProtocolMismatch => "protocol-mismatch",
            Self::DaemonUnavailable => "daemon-unavailable",
        }
    }
}

#[derive(Debug)]
pub(crate) struct LookupOutcome {
    pub(crate) response: LookupResponse,
    pub(crate) fallback_reason: Option<FallbackReason>,
    pub(crate) latency: Duration,
}

impl LookupOutcome {
    fn cached(response: LookupResponse, latency: Duration) -> Self {
        Self {
            response,
            fallback_reason: None,
            latency,
        }
    }

    fn direct(response: LookupResponse, reason: FallbackReason, latency: Duration) -> Self {
        Self {
            response,
            fallback_reason: Some(reason),
            latency,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_direct(&self) -> bool {
        self.response.status == CacheState::Bypassed
    }
}

#[derive(Clone, Debug)]
struct RuntimePaths {
    state_dir: PathBuf,
    socket: PathBuf,
    lock: PathBuf,
    pid: PathBuf,
}

impl RuntimePaths {
    fn discover() -> io::Result<Self> {
        let state_dir = crate::util::state_dir().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "QuietContext state directory unavailable",
            )
        })?;
        Ok(Self::from_state_dir(state_dir.join("repomap")))
    }

    #[cfg(test)]
    fn from_state_dir(state_dir: impl Into<PathBuf>) -> Self {
        Self::from_state_dir_inner(state_dir.into())
    }

    #[cfg(not(test))]
    fn from_state_dir(state_dir: impl Into<PathBuf>) -> Self {
        Self::from_state_dir_inner(state_dir.into())
    }

    fn from_state_dir_inner(state_dir: PathBuf) -> Self {
        Self {
            socket: state_dir.join(format!("repomap-{SOCKET_REVISION}.sock")),
            lock: state_dir.join(format!("repomap-{SOCKET_REVISION}.lock")),
            pid: state_dir.join(format!("repomap-{SOCKET_REVISION}.pid")),
            state_dir,
        }
    }

    fn ensure_private(&self) -> Result<(), ClientError> {
        std::fs::create_dir_all(&self.state_dir).map_err(ClientError::Io)?;
        let metadata = std::fs::symlink_metadata(&self.state_dir).map_err(ClientError::Io)?;
        if !metadata.file_type().is_dir() {
            return Err(ClientError::RuntimeUnsafe);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(ClientError::RuntimeUnsafe);
            }
            if metadata.mode() & 0o077 != 0 {
                std::fs::set_permissions(&self.state_dir, std::fs::Permissions::from_mode(0o700))
                    .map_err(ClientError::Io)?;
            }
        }
        self.validate_pid()
    }

    fn validate_pid(&self) -> Result<(), ClientError> {
        let metadata = match std::fs::symlink_metadata(&self.pid) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(ClientError::Io(error)),
        };
        if !metadata.file_type().is_file() {
            return Err(ClientError::RuntimeUnsafe);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
                return Err(ClientError::RuntimeUnsafe);
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn validate_socket(&self) -> Result<(), ClientError> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let metadata = match std::fs::symlink_metadata(&self.socket) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ClientError::RuntimeUnavailable)
            }
            Err(error) => return Err(ClientError::Io(error)),
        };
        if !metadata.file_type().is_socket()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(ClientError::RuntimeUnsafe);
        }
        Ok(())
    }
}

#[derive(Debug)]
enum ClientError {
    Io(io::Error),
    Timeout,
    Protocol(repomap_protocol::ProtocolError),
    RuntimeUnavailable,
    RuntimeUnsafe,
    PeerRejected,
    StartupBusy,
    Spawn,
}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(unix)]
struct StartupLock {
    file: std::fs::File,
}

#[cfg(unix)]
impl StartupLock {
    fn acquire(path: &Path, wait: Duration) -> Result<Self, ClientError> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .map_err(ClientError::Io)?;
        let metadata = file.metadata().map_err(ClientError::Io)?;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(ClientError::RuntimeUnsafe);
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(ClientError::Io)?;

        let started = Instant::now();
        loop {
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                return Ok(Self { file });
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() != io::ErrorKind::WouldBlock
                && error.raw_os_error() != Some(libc::EAGAIN)
            {
                return Err(ClientError::Io(error));
            }
            if started.elapsed() >= wait {
                return Err(ClientError::StartupBusy);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[cfg(unix)]
impl Drop for StartupLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(unix)]
fn start_daemon() -> Result<(), ClientError> {
    let executable = std::env::current_exe().map_err(|_| ClientError::Spawn)?;
    Command::new(executable)
        .arg(DAEMON_MODE_ARG)
        .env(DAEMON_MODE_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|_| ClientError::Spawn)
}

fn daemon_mode_arg_present_from<I>(mut args: I) -> bool
where
    I: Iterator<Item = std::ffi::OsString>,
{
    matches!(
        (args.next(), args.next()),
        (Some(arg), None) if arg == std::ffi::OsStr::new(DAEMON_MODE_ARG)
    )
}

fn daemon_mode_active_from<I>(args: I, env: Option<std::ffi::OsString>) -> bool
where
    I: Iterator<Item = std::ffi::OsString>,
{
    daemon_mode_arg_present_from(args)
        && env.is_some_and(|value| value == std::ffi::OsStr::new("1"))
}

pub(crate) fn daemon_mode_active() -> bool {
    daemon_mode_active_from(
        std::env::args_os().skip(1),
        std::env::var_os(DAEMON_MODE_ENV),
    )
}

pub(crate) fn lookup(request: LookupRequest, budget: ClientBudget) -> LookupOutcome {
    let started = Instant::now();
    let budget = budget.bounded();
    if request.validate().is_err() {
        return direct_lookup(request, FallbackReason::InvalidRequest, started);
    }
    if daemon_mode_active() {
        return direct_lookup(request, FallbackReason::DaemonMode, started);
    }

    #[cfg(not(unix))]
    {
        let _ = budget;
        return direct_lookup(request, FallbackReason::UnsupportedPlatform, started);
    }

    #[cfg(unix)]
    {
        let paths = match RuntimePaths::discover() {
            Ok(paths) => paths,
            Err(_) => return direct_lookup(request, FallbackReason::RuntimeUnavailable, started),
        };
        if let Err(error) = paths.ensure_private() {
            return direct_lookup(request, fallback_reason(&error), started);
        }

        match request_once(
            &paths,
            &request,
            budget.existing_connect_wait,
            budget.request_wait,
        ) {
            Ok(response) => return LookupOutcome::cached(response, started.elapsed()),
            Err(error) if protocol_timeout(&error) => {
                return direct_lookup(request, FallbackReason::RequestTimeout, started)
            }
            Err(error) if terminal_error(&error) => {
                return direct_lookup(request, fallback_reason(&error), started)
            }
            Err(_) => {}
        }

        match StartupLock::acquire(&paths.lock, budget.startup_lock_wait) {
            Ok(lock) => {
                match request_once(
                    &paths,
                    &request,
                    budget.existing_connect_wait,
                    budget.request_wait,
                ) {
                    Ok(response) => return LookupOutcome::cached(response, started.elapsed()),
                    Err(error) if protocol_timeout(&error) => {
                        return direct_lookup(request, FallbackReason::RequestTimeout, started)
                    }
                    Err(error) if terminal_error(&error) => {
                        return direct_lookup(request, fallback_reason(&error), started)
                    }
                    Err(_) => {}
                }

                if let Err(error) = start_daemon() {
                    return direct_lookup(request, fallback_reason(&error), started);
                }

                let deadline = Instant::now() + budget.startup_wait;
                let outcome = match wait_for_daemon(&paths, &request, budget.request_wait, deadline)
                {
                    Ok(response) => LookupOutcome::cached(response, started.elapsed()),
                    Err(error) => {
                        let reason = if matches!(error, ClientError::Timeout) {
                            FallbackReason::StartupTimeout
                        } else {
                            fallback_reason(&error)
                        };
                        direct_lookup(request, reason, started)
                    }
                };
                drop(lock);
                outcome
            }
            Err(error) => {
                let startup_reason = fallback_reason(&error);
                if terminal_error(&error) && !matches!(error, ClientError::StartupBusy) {
                    return direct_lookup(request, startup_reason, started);
                }

                let deadline = Instant::now() + budget.startup_wait;
                match wait_for_daemon(&paths, &request, budget.request_wait, deadline) {
                    Ok(response) => LookupOutcome::cached(response, started.elapsed()),
                    Err(error) => {
                        let reason = if matches!(error, ClientError::Timeout) {
                            startup_reason
                        } else {
                            fallback_reason(&error)
                        };
                        direct_lookup(request, reason, started)
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
fn wait_for_daemon(
    paths: &RuntimePaths,
    request: &LookupRequest,
    request_wait: Duration,
    deadline: Instant,
) -> Result<LookupResponse, ClientError> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ClientError::Timeout);
        }
        let operation_deadline = (Instant::now() + request_wait).min(deadline);
        let connect_wait = remaining.min(EXISTING_CONNECT_WAIT);
        match request_once_until(paths, request, connect_wait, operation_deadline) {
            Ok(response) => return Ok(response),
            Err(error) if terminal_error(&error) => return Err(error),
            Err(_) => {}
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            std::thread::sleep(remaining.min(Duration::from_millis(1)));
        }
    }
}

#[cfg(unix)]
fn request_once(
    paths: &RuntimePaths,
    request: &LookupRequest,
    connect_wait: Duration,
    request_wait: Duration,
) -> Result<LookupResponse, ClientError> {
    let deadline = Instant::now() + request_wait;
    request_once_until(paths, request, connect_wait, deadline)
}

#[cfg(unix)]
fn request_once_until(
    paths: &RuntimePaths,
    request: &LookupRequest,
    connect_wait: Duration,
    deadline: Instant,
) -> Result<LookupResponse, ClientError> {
    paths.validate_socket()?;
    let connect_deadline = (Instant::now() + connect_wait).min(deadline);
    let mut stream = connect_unix_until(&paths.socket, connect_deadline)?;
    if !same_uid(&stream)? {
        return Err(ClientError::PeerRejected);
    }
    stream.set_nonblocking(true).map_err(ClientError::Io)?;
    let frame = repomap_protocol::encode_request(request).map_err(ClientError::Protocol)?;
    write_until(&mut stream, &frame, deadline)?;
    let response = read_response_until(&mut stream, request.map_config.max_bytes, deadline)?;
    response.validate().map_err(ClientError::Protocol)?;
    Ok(response)
}

#[cfg(unix)]
fn write_until(
    stream: &mut std::os::unix::net::UnixStream,
    frame: &[u8],
    deadline: Instant,
) -> Result<(), ClientError> {
    let mut written = 0;
    while written < frame.len() {
        if Instant::now() >= deadline {
            return Err(ClientError::Timeout);
        }
        match stream.write(&frame[written..]) {
            Ok(0) => {
                return Err(protocol_io_error(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "daemon closed request socket",
                )))
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                use std::os::fd::AsRawFd;
                wait_io(stream.as_raw_fd(), libc::POLLOUT, deadline)?;
            }
            Err(error) => return Err(protocol_io_error(error)),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_response_until(
    stream: &mut std::os::unix::net::UnixStream,
    effective_output_cap: usize,
    deadline: Instant,
) -> Result<LookupResponse, ClientError> {
    let mut frame = vec![0_u8; repomap_protocol::FRAME_HEADER_BYTES];
    read_exact_until(stream, &mut frame, deadline)?;
    let length = u32::from_be_bytes(
        frame[..repomap_protocol::FRAME_HEADER_BYTES]
            .try_into()
            .expect("frame header has fixed width"),
    ) as usize;
    let maximum = repomap_protocol::max_response_frame_bytes(effective_output_cap);
    if length > maximum {
        return Err(ClientError::Protocol(
            repomap_protocol::ProtocolError::FrameTooLarge { length, maximum },
        ));
    }
    frame.resize(
        repomap_protocol::FRAME_HEADER_BYTES
            .checked_add(length)
            .expect("validated response frame length fits usize"),
        0,
    );
    read_exact_until(
        stream,
        &mut frame[repomap_protocol::FRAME_HEADER_BYTES..],
        deadline,
    )?;
    repomap_protocol::decode_response(&frame, effective_output_cap).map_err(ClientError::Protocol)
}

#[cfg(unix)]
fn read_exact_until(
    stream: &mut std::os::unix::net::UnixStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<(), ClientError> {
    let expected = buffer.len();
    let mut read = 0;
    while read < buffer.len() {
        if Instant::now() >= deadline {
            return Err(ClientError::Timeout);
        }
        match stream.read(&mut buffer[read..]) {
            Ok(0) => {
                return Err(ClientError::Protocol(
                    repomap_protocol::ProtocolError::TruncatedFrame {
                        expected,
                        actual: read,
                    },
                ))
            }
            Ok(count) => read += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                use std::os::fd::AsRawFd;
                wait_io(stream.as_raw_fd(), libc::POLLIN, deadline)?;
            }
            Err(error) => return Err(protocol_io_error(error)),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn protocol_io_error(error: io::Error) -> ClientError {
    if error.kind() == io::ErrorKind::TimedOut {
        ClientError::Timeout
    } else {
        ClientError::Protocol(repomap_protocol::ProtocolError::Io(error))
    }
}

#[cfg(unix)]
fn wait_io(
    fd: std::os::fd::RawFd,
    events: libc::c_short,
    deadline: Instant,
) -> Result<(), ClientError> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ClientError::Timeout);
        }
        let mut poll = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, poll_timeout(remaining)) };
        if result == 0 {
            continue;
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(protocol_io_error(error));
        }
        if poll.revents & libc::POLLNVAL != 0 {
            return Err(protocol_io_error(io::Error::new(
                io::ErrorKind::NotConnected,
                "daemon socket became invalid",
            )));
        }
        if poll.revents & (events | libc::POLLERR | libc::POLLHUP) != 0 {
            return Ok(());
        }
    }
}

#[cfg(unix)]
fn protocol_timeout(error: &ClientError) -> bool {
    matches!(error, ClientError::Timeout)
        || matches!(
            error,
            ClientError::Protocol(repomap_protocol::ProtocolError::Io(error))
                if error.kind() == io::ErrorKind::TimedOut
        )
}

#[cfg(unix)]
fn terminal_error(error: &ClientError) -> bool {
    match error {
        ClientError::Protocol(repomap_protocol::ProtocolError::Io(error))
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut
                    | io::ErrorKind::WouldBlock
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
            ) =>
        {
            false
        }
        ClientError::RuntimeUnsafe
        | ClientError::PeerRejected
        | ClientError::Protocol(_)
        | ClientError::Spawn => true,
        _ => false,
    }
}

#[cfg(unix)]
fn fallback_reason(error: &ClientError) -> FallbackReason {
    match error {
        ClientError::RuntimeUnavailable => FallbackReason::RuntimeUnavailable,
        ClientError::RuntimeUnsafe => FallbackReason::RuntimeUnsafe,
        ClientError::PeerRejected => FallbackReason::PeerRejected,
        ClientError::StartupBusy => FallbackReason::StartupBusy,
        ClientError::Spawn => FallbackReason::SpawnFailed,
        ClientError::Protocol(repomap_protocol::ProtocolError::Io(error))
            if error.kind() == io::ErrorKind::TimedOut =>
        {
            FallbackReason::RequestTimeout
        }
        ClientError::Protocol(_) => FallbackReason::ProtocolMismatch,
        ClientError::Timeout => FallbackReason::RequestTimeout,
        ClientError::Io(error) if error.kind() == io::ErrorKind::TimedOut => {
            FallbackReason::RequestTimeout
        }
        ClientError::Io(_) => FallbackReason::DaemonUnavailable,
    }
}

fn direct_lookup(
    request: LookupRequest,
    reason: FallbackReason,
    started: Instant,
) -> LookupOutcome {
    let root = PathBuf::from(&request.canonical_root);
    let config: MapConfig = request.map_config.into();
    let result = match request.operation {
        repomap_protocol::LookupOperation::Map => repomap::build_map_direct(&root, &config),
        repomap_protocol::LookupOperation::Sym => {
            repomap::build_sym_direct(request.query.as_deref().unwrap_or_default(), &root, &config)
        }
        repomap_protocol::LookupOperation::Refs => {
            repomap::build_refs_direct(request.query.as_deref().unwrap_or_default(), &root, &config)
        }
    };
    let response = response_from_result(
        request.operation,
        request.query.as_deref(),
        result,
        CacheState::Bypassed,
        0,
        LookupTimings {
            total_us: elapsed_us(started.elapsed()),
            ..LookupTimings::default()
        },
    );
    LookupOutcome::direct(response, reason, started.elapsed())
}

fn response_from_result(
    operation: repomap_protocol::LookupOperation,
    query: Option<&str>,
    result: repomap::ScanResult,
    status: CacheState,
    generation: u64,
    timings: LookupTimings,
) -> LookupResponse {
    let (stdout, stderr, exit_code) = match result {
        Ok(Some(output)) => (output, String::new(), 0),
        Ok(None) => match operation {
            repomap_protocol::LookupOperation::Map => (
                String::new(),
                "qc repo map: no source files found\n".to_owned(),
                1,
            ),
            repomap_protocol::LookupOperation::Sym => (
                String::new(),
                format!("qc repo symbol: {} not found\n", query.unwrap_or_default()),
                1,
            ),
            repomap_protocol::LookupOperation::Refs => (
                String::new(),
                format!(
                    "qc repo references: {} not found\n",
                    query.unwrap_or_default()
                ),
                1,
            ),
        },
        Err(error) => {
            let command = match operation {
                repomap_protocol::LookupOperation::Map => "map",
                repomap_protocol::LookupOperation::Sym => "sym",
                repomap_protocol::LookupOperation::Refs => "refs",
            };
            (String::new(), format!("qc repo {command}: {error}\n"), 1)
        }
    };
    LookupResponse {
        version: PROTOCOL_VERSION,
        status,
        stdout,
        stderr,
        exit_code,
        generation,
        timings,
    }
}

fn elapsed_us(duration: Duration) -> u32 {
    duration.as_micros().min(u32::MAX as u128) as u32
}

#[cfg(unix)]
fn same_uid(stream: &std::os::unix::net::UnixStream) -> Result<bool, ClientError> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let mut credential: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credential as *mut libc::ucred).cast(),
                &mut length,
            )
        };
        if result != 0 {
            return Err(ClientError::Io(io::Error::last_os_error()));
        }
        Ok(credential.uid == unsafe { libc::geteuid() })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = stream;
        Ok(true)
    }
}

#[cfg(unix)]
#[cfg(test)]
fn connect_unix(
    path: &Path,
    timeout: Duration,
) -> Result<std::os::unix::net::UnixStream, ClientError> {
    connect_unix_until(path, Instant::now() + timeout)
}

#[cfg(unix)]
fn connect_unix_until(
    path: &Path,
    deadline: Instant,
) -> Result<std::os::unix::net::UnixStream, ClientError> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixStream;

    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() {
        return Err(ClientError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "daemon socket path is too long",
        )));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (target, source) in address.sun_path.iter_mut().zip(bytes.iter().copied()) {
        *target = source as libc::c_char;
    }
    let address_len =
        (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(ClientError::Io(io::Error::last_os_error()));
    }
    let mut owned = OwnedFd(fd);
    set_close_on_exec(fd)?;
    set_nonblocking(fd, true)?;
    let result = unsafe {
        libc::connect(
            fd,
            (&address as *const libc::sockaddr_un).cast(),
            address_len,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(ClientError::Io(error));
        }
        wait_connect(fd, deadline)?;
        let mut socket_error = 0_i32;
        let mut socket_error_len = std::mem::size_of::<i32>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut socket_error as *mut i32).cast(),
                &mut socket_error_len,
            )
        };
        if result != 0 {
            return Err(ClientError::Io(io::Error::last_os_error()));
        }
        if socket_error != 0 {
            return Err(ClientError::Io(io::Error::from_raw_os_error(socket_error)));
        }
    }
    set_nonblocking(fd, false)?;
    let fd = owned.take();
    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}

#[cfg(unix)]
fn set_close_on_exec(fd: std::os::fd::RawFd) -> Result<(), ClientError> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(ClientError::Io(io::Error::last_os_error()));
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(ClientError::Io(io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(unix)]
fn set_nonblocking(fd: std::os::fd::RawFd, enabled: bool) -> Result<(), ClientError> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(ClientError::Io(io::Error::last_os_error()));
    }
    let next = if enabled {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFL, next) } < 0 {
        return Err(ClientError::Io(io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(unix)]
fn wait_connect(fd: std::os::fd::RawFd, deadline: Instant) -> Result<(), ClientError> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ClientError::Timeout);
        }
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, poll_timeout(remaining)) };
        if result == 0 {
            continue;
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(ClientError::Io(error));
        }
        if poll.revents & libc::POLLNVAL != 0 {
            return Err(ClientError::Io(io::Error::new(
                io::ErrorKind::NotConnected,
                "daemon socket became invalid",
            )));
        }
        return Ok(());
    }
}

#[cfg(unix)]
fn poll_timeout(timeout: Duration) -> libc::c_int {
    timeout.as_millis().min(i32::MAX as u128) as libc::c_int
}

#[cfg(unix)]
struct OwnedFd(std::os::fd::RawFd);

#[cfg(unix)]
impl OwnedFd {
    fn take(&mut self) -> std::os::fd::RawFd {
        let fd = self.0;
        self.0 = -1;
        fd
    }
}

#[cfg(unix)]
impl Drop for OwnedFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe {
                libc::close(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repomap_protocol::{EffectiveMapConfig, LookupOperation};
    use std::fs;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn root() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("qc-client-{suffix}"));
        fs::create_dir_all(&root).expect("root");
        root
    }

    fn request(root: &Path, operation: LookupOperation, query: Option<&str>) -> LookupRequest {
        LookupRequest::new(
            operation,
            root.to_string_lossy().to_string(),
            query.map(str::to_owned),
            EffectiveMapConfig::default(),
        )
        .expect("request")
    }

    #[test]
    fn daemon_activation_requires_exact_marker_and_environment() {
        let marker = std::ffi::OsString::from(DAEMON_MODE_ARG);
        let enabled = Some(std::ffi::OsString::from("1"));
        assert!(daemon_mode_active_from(
            vec![marker.clone()].into_iter(),
            enabled.clone()
        ));
        assert!(!daemon_mode_active_from(
            vec![marker.clone()].into_iter(),
            None
        ));
        assert!(!daemon_mode_active_from(
            vec![marker.clone(), std::ffi::OsString::from("extra")].into_iter(),
            enabled.clone()
        ));
        assert!(!daemon_mode_active_from(
            vec![std::ffi::OsString::from("git"), marker].into_iter(),
            enabled.clone()
        ));
        assert!(!daemon_mode_active_from(
            vec![std::ffi::OsString::from(DAEMON_MODE_ARG)].into_iter(),
            Some(std::ffi::OsString::from("0"))
        ));
    }

    #[test]
    fn budget_defaults_are_contract_bounds() {
        let budget = ClientBudget::default();
        assert_eq!(budget.startup_lock_wait, Duration::from_millis(5));
        assert_eq!(budget.existing_connect_wait, Duration::from_millis(2));
        assert_eq!(budget.startup_wait, Duration::from_millis(100));
        assert_eq!(budget.request_wait, Duration::from_millis(25));
        assert_eq!(
            ClientBudget::new(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1)
            )
            .bounded(),
            budget
        );
    }

    #[test]
    fn direct_fallback_preserves_lookup_error_shape() {
        let root = root();
        let outcome = direct_lookup(
            request(&root, LookupOperation::Sym, Some("missing")),
            FallbackReason::RuntimeUnavailable,
            Instant::now(),
        );
        assert!(outcome.is_direct());
        assert_eq!(outcome.response.status, CacheState::Bypassed);
        assert_eq!(outcome.response.exit_code, 1);
        assert_eq!(
            outcome.response.stderr,
            "qc repo symbol: missing not found\n"
        );
        assert_eq!(
            outcome.fallback_reason,
            Some(FallbackReason::RuntimeUnavailable)
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn direct_fallback_map_output_matches_scanner() {
        let root = root();
        let path = root.join("main.rs");
        let mut file = fs::File::create(path).expect("file");
        file.write_all(b"pub fn stable() {}\n").expect("write");
        let request = request(&root, LookupOperation::Map, None);
        let direct = crate::repomap::build_map_direct(&root, &MapConfig::default())
            .expect("scan")
            .expect("output");
        let outcome = direct_lookup(request, FallbackReason::DaemonUnavailable, Instant::now());
        assert_eq!(outcome.response.stdout, direct);
        assert_eq!(outcome.response.stderr, "");
        assert_eq!(outcome.response.exit_code, 0);
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn runtime_socket_and_lock_names_include_revision() {
        let paths = RuntimePaths::from_state_dir("/tmp/qc-client-state");
        assert!(paths
            .socket
            .ends_with(format!("repomap-{SOCKET_REVISION}.sock")));
        assert!(paths
            .lock
            .ends_with(format!("repomap-{SOCKET_REVISION}.lock")));
        assert!(paths
            .pid
            .ends_with(format!("repomap-{SOCKET_REVISION}.pid")));
    }

    #[cfg(unix)]
    #[test]
    fn startup_lock_is_single_flight_and_bounded() {
        let root = root();
        let paths = RuntimePaths::from_state_dir(root.join("state"));
        fs::create_dir_all(&paths.state_dir).expect("state");
        let first = StartupLock::acquire(&paths.lock, Duration::from_millis(5)).expect("first");
        let started = Instant::now();
        let second = StartupLock::acquire(&paths.lock, Duration::from_millis(5));
        assert!(matches!(second, Err(ClientError::StartupBusy)));
        assert!(started.elapsed() < Duration::from_millis(100));
        drop(first);
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn peer_validation_accepts_same_uid_socket() {
        use std::os::unix::net::UnixListener;
        let root = root();
        let socket = root.join("peer.sock");
        let listener = UnixListener::bind(&socket).expect("listener");
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).expect("mode");
        let stream = connect_unix(&socket, Duration::from_millis(2)).expect("connect");
        assert!(same_uid(&stream).expect("peer credential"));
        let _ = listener.accept().expect("accept");
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn protocol_revision_mismatch_is_terminal() {
        let error = ClientError::Protocol(repomap_protocol::ProtocolError::UnsupportedVersion(99));
        assert!(terminal_error(&error));
        assert_eq!(fallback_reason(&error), FallbackReason::ProtocolMismatch);
    }

    #[cfg(unix)]
    #[test]
    fn startup_wait_has_absolute_deadline() {
        let root = root();
        let paths = RuntimePaths::from_state_dir(root.join("state"));
        fs::create_dir_all(&paths.state_dir).expect("state");
        let request = request(&root, LookupOperation::Map, None);
        let deadline = Instant::now() + Duration::from_millis(5);
        assert!(matches!(
            wait_for_daemon(&paths, &request, Duration::from_millis(25), deadline),
            Err(ClientError::Timeout)
        ));
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn response_frame_parts_share_one_deadline() {
        use std::os::unix::net::UnixStream;
        use std::thread;

        let (mut client, mut server) = UnixStream::pair().expect("stream pair");
        client.set_nonblocking(true).expect("nonblocking client");
        let response = LookupResponse {
            version: PROTOCOL_VERSION,
            status: CacheState::Hit,
            stdout: "x".repeat(32),
            stderr: String::new(),
            exit_code: 0,
            generation: 1,
            timings: LookupTimings::default(),
        };
        let frame = repomap_protocol::encode_response(&response, 1024).expect("response frame");
        let writer = thread::spawn(move || {
            server
                .write_all(&frame[..repomap_protocol::FRAME_HEADER_BYTES])
                .expect("frame header");
            for byte in &frame[repomap_protocol::FRAME_HEADER_BYTES..] {
                if server.write_all(std::slice::from_ref(byte)).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        let started = Instant::now();
        let result = read_response_until(&mut client, 1024, started + Duration::from_millis(20));
        assert!(matches!(result, Err(ClientError::Timeout)));
        assert!(started.elapsed() < Duration::from_millis(100));
        drop(client);
        writer.join().expect("writer");
    }
}
