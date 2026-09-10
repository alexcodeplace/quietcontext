use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;

pub const PROTOCOL_VERSION: u32 = 1;
pub const FRAME_HEADER_BYTES: usize = std::mem::size_of::<u32>();
pub const MAX_REQUEST_FRAME_BYTES: usize = 64 * 1024;
pub const RESPONSE_FRAME_HEADROOM_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LookupOperation {
    Map,
    Sym,
    Refs,
}

impl LookupOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Map => "map",
            Self::Sym => "sym",
            Self::Refs => "refs",
        }
    }

    fn requires_query(self) -> bool {
        !matches!(self, Self::Map)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheState {
    Hit,
    Refreshed,
    Reconciled,
    Bypassed,
}

impl CacheState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Refreshed => "refreshed",
            Self::Reconciled => "reconciled",
            Self::Bypassed => "bypassed",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LookupTimings {
    pub queue_us: u32,
    pub reconcile_us: u32,
    pub render_us: u32,
    pub total_us: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveMapConfig {
    pub max_bytes: usize,
    pub max_files: usize,
    pub max_file_bytes: usize,
    pub refs_max_per_file: usize,
    pub refs_max_total: usize,
}

impl Default for EffectiveMapConfig {
    fn default() -> Self {
        Self::from(&crate::config::MapConfig::default())
    }
}

impl From<&crate::config::MapConfig> for EffectiveMapConfig {
    fn from(config: &crate::config::MapConfig) -> Self {
        Self {
            max_bytes: config.max_bytes,
            max_files: config.max_files,
            max_file_bytes: config.max_file_bytes,
            refs_max_per_file: config.refs_max_per_file,
            refs_max_total: config.refs_max_total,
        }
    }
}

impl From<EffectiveMapConfig> for crate::config::MapConfig {
    fn from(config: EffectiveMapConfig) -> Self {
        Self {
            max_bytes: config.max_bytes,
            max_files: config.max_files,
            max_file_bytes: config.max_file_bytes,
            refs_max_per_file: config.refs_max_per_file,
            refs_max_total: config.refs_max_total,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LookupRequest {
    pub version: u32,
    pub operation: LookupOperation,
    pub canonical_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    pub map_config: EffectiveMapConfig,
}

impl LookupRequest {
    #[cfg(test)]
    pub fn new(
        operation: LookupOperation,
        canonical_root: impl Into<String>,
        query: Option<String>,
        map_config: EffectiveMapConfig,
    ) -> Result<Self, ProtocolError> {
        let request = Self {
            version: PROTOCOL_VERSION,
            operation,
            canonical_root: canonical_root.into(),
            query,
            map_config,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_version(self.version)?;
        validate_root(&self.canonical_root)?;
        match (self.operation.requires_query(), self.query.as_deref()) {
            (false, None) => Ok(()),
            (false, Some(_)) => Err(ProtocolError::InvalidQuery),
            (true, Some(query)) if valid_query(query) => Ok(()),
            (true, _) => Err(ProtocolError::InvalidQuery),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LookupResponse {
    pub version: u32,
    #[serde(alias = "cache_state")]
    pub status: CacheState,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub generation: u64,
    #[serde(default)]
    pub timings: LookupTimings,
}

impl LookupResponse {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_version(self.version)
    }
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    TruncatedFrame { expected: usize, actual: usize },
    FrameLengthMismatch { declared: usize, actual: usize },
    FrameTooLarge { length: usize, maximum: usize },
    InvalidUtf8,
    InvalidJson(serde_json::Error),
    UnsupportedVersion(u32),
    NonCanonicalRoot,
    InvalidQuery,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "protocol I/O error: {err}"),
            Self::TruncatedFrame { expected, actual } => {
                write!(
                    f,
                    "truncated protocol frame: expected {expected} bytes, got {actual}"
                )
            }
            Self::FrameLengthMismatch { declared, actual } => write!(
                f,
                "protocol frame length mismatch: header declares {declared} bytes, got {actual}"
            ),
            Self::FrameTooLarge { length, maximum } => {
                write!(
                    f,
                    "protocol frame too large: {length} bytes exceeds {maximum}"
                )
            }
            Self::InvalidUtf8 => write!(f, "protocol frame is not valid UTF-8"),
            Self::InvalidJson(err) => write!(f, "invalid protocol JSON: {err}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported protocol version: {version}")
            }
            Self::NonCanonicalRoot => write!(f, "protocol root is not canonical"),
            Self::InvalidQuery => write!(f, "invalid lookup query"),
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::InvalidJson(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for ProtocolError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn validate_version(version: u32) -> Result<(), ProtocolError> {
    if version == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtocolError::UnsupportedVersion(version))
    }
}

fn validate_root(root: &str) -> Result<(), ProtocolError> {
    let path = Path::new(root);
    if root.is_empty() || root.contains('\0') || !path.is_absolute() {
        return Err(ProtocolError::NonCanonicalRoot);
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| ProtocolError::NonCanonicalRoot)?;
    if canonical != path {
        return Err(ProtocolError::NonCanonicalRoot);
    }
    Ok(())
}

fn valid_query(query: &str) -> bool {
    !query.is_empty() && !query.chars().any(char::is_control)
}

fn response_frame_limit(effective_output_cap: usize) -> usize {
    effective_output_cap
        .saturating_add(RESPONSE_FRAME_HEADROOM_BYTES)
        .min(u32::MAX as usize)
}

pub fn max_response_frame_bytes(effective_output_cap: usize) -> usize {
    response_frame_limit(effective_output_cap)
}

fn ensure_frame_limit(length: usize, maximum: usize) -> Result<(), ProtocolError> {
    if length > maximum || length > u32::MAX as usize {
        return Err(ProtocolError::FrameTooLarge { length, maximum });
    }
    Ok(())
}

fn serialize_frame<T: Serialize>(value: &T, maximum: usize) -> Result<Vec<u8>, ProtocolError> {
    let payload = serde_json::to_vec(value).map_err(ProtocolError::InvalidJson)?;
    ensure_frame_limit(payload.len(), maximum)?;
    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn deserialize_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, ProtocolError> {
    std::str::from_utf8(payload).map_err(|_| ProtocolError::InvalidUtf8)?;
    serde_json::from_slice(payload).map_err(ProtocolError::InvalidJson)
}

fn payload_from_frame(frame: &[u8], maximum: usize) -> Result<&[u8], ProtocolError> {
    if frame.len() < FRAME_HEADER_BYTES {
        return Err(ProtocolError::TruncatedFrame {
            expected: FRAME_HEADER_BYTES,
            actual: frame.len(),
        });
    }
    let declared = u32::from_be_bytes(
        frame[..FRAME_HEADER_BYTES]
            .try_into()
            .expect("frame header has fixed width"),
    ) as usize;
    ensure_frame_limit(declared, maximum)?;
    let actual = frame.len() - FRAME_HEADER_BYTES;
    if actual != declared {
        return Err(ProtocolError::FrameLengthMismatch { declared, actual });
    }
    Ok(&frame[FRAME_HEADER_BYTES..])
}

#[cfg(test)]
pub fn encode_frame<T: Serialize>(value: &T, maximum: usize) -> Result<Vec<u8>, ProtocolError> {
    serialize_frame(value, maximum)
}

pub fn decode_frame<T: DeserializeOwned>(frame: &[u8], maximum: usize) -> Result<T, ProtocolError> {
    deserialize_payload(payload_from_frame(frame, maximum)?)
}

pub fn write_frame<W: Write>(
    writer: &mut W,
    payload: &[u8],
    maximum: usize,
) -> Result<(), ProtocolError> {
    ensure_frame_limit(payload.len(), maximum)?;
    writer.write_all(&(payload.len() as u32).to_be_bytes())?;
    writer.write_all(payload)?;
    Ok(())
}

pub fn read_frame<R: Read>(reader: &mut R, maximum: usize) -> Result<Vec<u8>, ProtocolError> {
    let mut header = [0_u8; FRAME_HEADER_BYTES];
    read_exact_frame_part(reader, &mut header, FRAME_HEADER_BYTES)?;
    let length = u32::from_be_bytes(header) as usize;
    ensure_frame_limit(length, maximum)?;
    let mut payload = vec![0_u8; length];
    read_exact_frame_part(reader, &mut payload, length)?;
    Ok(payload)
}

fn read_exact_frame_part<R: Read>(
    reader: &mut R,
    buffer: &mut [u8],
    expected: usize,
) -> Result<(), ProtocolError> {
    let mut read = 0;
    while read < buffer.len() {
        match reader.read(&mut buffer[read..]) {
            Ok(0) => {
                return Err(ProtocolError::TruncatedFrame {
                    expected,
                    actual: read,
                });
            }
            Ok(count) => read += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ProtocolError::Io(error)),
        }
    }
    Ok(())
}

pub fn encode_request(request: &LookupRequest) -> Result<Vec<u8>, ProtocolError> {
    request.validate()?;
    serialize_frame(request, MAX_REQUEST_FRAME_BYTES)
}

#[cfg(test)]
pub fn decode_request(frame: &[u8]) -> Result<LookupRequest, ProtocolError> {
    let request = decode_frame::<LookupRequest>(frame, MAX_REQUEST_FRAME_BYTES)?;
    request.validate()?;
    Ok(request)
}

pub fn decode_request_body(payload: &[u8]) -> Result<LookupRequest, ProtocolError> {
    ensure_frame_limit(payload.len(), MAX_REQUEST_FRAME_BYTES)?;
    let request: LookupRequest = deserialize_payload(payload)?;
    request.validate()?;
    Ok(request)
}

pub fn read_request<R: Read>(reader: &mut R) -> Result<LookupRequest, ProtocolError> {
    let payload = read_frame(reader, MAX_REQUEST_FRAME_BYTES)?;
    decode_request_body(&payload)
}

#[cfg(test)]
pub fn encode_response(
    response: &LookupResponse,
    effective_output_cap: usize,
) -> Result<Vec<u8>, ProtocolError> {
    response.validate()?;
    serialize_frame(response, response_frame_limit(effective_output_cap))
}

pub fn decode_response(
    frame: &[u8],
    effective_output_cap: usize,
) -> Result<LookupResponse, ProtocolError> {
    let response =
        decode_frame::<LookupResponse>(frame, response_frame_limit(effective_output_cap))?;
    response.validate()?;
    Ok(response)
}

#[cfg(test)]
pub fn decode_response_body(
    payload: &[u8],
    effective_output_cap: usize,
) -> Result<LookupResponse, ProtocolError> {
    let maximum = response_frame_limit(effective_output_cap);
    ensure_frame_limit(payload.len(), maximum)?;
    let response: LookupResponse = deserialize_payload(payload)?;
    response.validate()?;
    Ok(response)
}

pub fn write_response<W: Write>(
    writer: &mut W,
    response: &LookupResponse,
    effective_output_cap: usize,
) -> Result<(), ProtocolError> {
    response.validate()?;
    let payload = serde_json::to_vec(response).map_err(ProtocolError::InvalidJson)?;
    write_frame(writer, &payload, response_frame_limit(effective_output_cap))
}

#[cfg(test)]
pub fn read_response<R: Read>(
    reader: &mut R,
    effective_output_cap: usize,
) -> Result<LookupResponse, ProtocolError> {
    let payload = read_frame(reader, response_frame_limit(effective_output_cap))?;
    decode_response_body(&payload, effective_output_cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn root() -> String {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("qc-repomap-protocol-{suffix}"));
        fs::create_dir(&path).expect("create protocol test root");
        path.to_string_lossy().into_owned()
    }

    fn request(operation: LookupOperation, query: Option<&str>, root: &str) -> LookupRequest {
        LookupRequest::new(
            operation,
            root,
            query.map(str::to_owned),
            EffectiveMapConfig::default(),
        )
        .expect("valid request")
    }

    fn response() -> LookupResponse {
        LookupResponse {
            version: PROTOCOL_VERSION,
            status: CacheState::Hit,
            stdout: "map output\n".to_owned(),
            stderr: String::new(),
            exit_code: 0,
            generation: 4,
            timings: LookupTimings {
                queue_us: 1,
                reconcile_us: 2,
                render_us: 3,
                total_us: 6,
            },
        }
    }

    #[test]
    fn request_round_trip_preserves_contract() {
        let root = root();
        let original = request(LookupOperation::Refs, Some("needle"), &root);
        let frame = encode_request(&original).expect("encode request");
        assert_eq!(
            u32::from_be_bytes(frame[..FRAME_HEADER_BYTES].try_into().unwrap()) as usize,
            frame.len() - FRAME_HEADER_BYTES
        );
        let decoded = decode_request(&frame).expect("decode request");
        assert_eq!(decoded, original);
        fs::remove_dir(root).expect("remove protocol test root");
    }

    #[test]
    fn response_round_trip_and_stream_helpers_preserve_contract() {
        let original = response();
        let frame = encode_response(&original, 4096).expect("encode response");
        assert_eq!(
            decode_response(&frame, 4096).expect("decode response"),
            original
        );

        let mut stream = Vec::new();
        write_response(&mut stream, &original, 4096).expect("write response");
        let decoded = read_response(&mut stream.as_slice(), 4096).expect("read response");
        assert_eq!(decoded, original);
    }

    #[test]
    fn frame_prefix_is_unsigned_big_endian() {
        let frame = encode_frame(&"x", 64).expect("encode frame");
        assert_eq!(&frame[..FRAME_HEADER_BYTES], &[0, 0, 0, 3]);
        assert_eq!(
            decode_frame::<String>(&frame, 64).expect("decode frame"),
            "x"
        );
    }

    #[test]
    fn request_rejects_version_query_and_root_violations() {
        let root = root();
        let mut unsupported = request(LookupOperation::Map, None, &root);
        unsupported.version = PROTOCOL_VERSION + 1;
        assert!(matches!(
            unsupported.validate(),
            Err(ProtocolError::UnsupportedVersion(_))
        ));

        let map_query = LookupRequest {
            version: PROTOCOL_VERSION,
            operation: LookupOperation::Map,
            canonical_root: root.clone(),
            query: Some("unexpected".to_owned()),
            map_config: EffectiveMapConfig::default(),
        };
        assert!(matches!(
            map_query.validate(),
            Err(ProtocolError::InvalidQuery)
        ));

        let empty_sym = LookupRequest {
            version: PROTOCOL_VERSION,
            operation: LookupOperation::Sym,
            canonical_root: root.clone(),
            query: Some(String::new()),
            map_config: EffectiveMapConfig::default(),
        };
        assert!(matches!(
            empty_sym.validate(),
            Err(ProtocolError::InvalidQuery)
        ));

        let noncanonical = format!("{root}/..");
        let invalid_root = LookupRequest {
            version: PROTOCOL_VERSION,
            operation: LookupOperation::Map,
            canonical_root: noncanonical,
            query: None,
            map_config: EffectiveMapConfig::default(),
        };
        assert!(matches!(
            invalid_root.validate(),
            Err(ProtocolError::NonCanonicalRoot)
        ));
        fs::remove_dir(root).expect("remove protocol test root");
    }

    #[test]
    fn unknown_fields_and_operations_are_rejected() {
        let root = root();
        let request = request(LookupOperation::Map, None, &root);
        let mut value = serde_json::to_value(request).expect("request value");
        value
            .as_object_mut()
            .expect("request object")
            .insert("unexpected".to_owned(), serde_json::Value::Null);
        let payload = serde_json::to_vec(&value).expect("request JSON");
        assert!(matches!(
            decode_request_body(&payload),
            Err(ProtocolError::InvalidJson(_))
        ));

        let operation = format!(
            r#"{{"version":1,"operation":"bogus","canonical_root":{},"map_config":{{"max_bytes":1,"max_files":1,"max_file_bytes":1,"refs_max_per_file":1,"refs_max_total":1}}}}"#,
            serde_json::to_string(&root).unwrap()
        );
        assert!(matches!(
            decode_request_body(operation.as_bytes()),
            Err(ProtocolError::InvalidJson(_))
        ));
        fs::remove_dir(root).expect("remove protocol test root");
    }

    #[test]
    fn malformed_and_oversized_frames_fail_closed() {
        assert!(matches!(
            decode_frame::<String>(&[0, 0, 0], 64),
            Err(ProtocolError::TruncatedFrame { .. })
        ));
        assert!(matches!(
            decode_frame::<String>(&[0, 0, 0, 5, b'x'], 4),
            Err(ProtocolError::FrameTooLarge { .. })
        ));
        assert!(matches!(
            decode_frame::<String>(&[0, 0, 0, 1, b'x', b'y'], 4),
            Err(ProtocolError::FrameLengthMismatch { .. })
        ));

        let mut invalid_utf8 = vec![0, 0, 0, 1, 0xff];
        assert!(matches!(
            decode_frame::<String>(&invalid_utf8, 64),
            Err(ProtocolError::InvalidUtf8)
        ));
        invalid_utf8[4] = b'x';
        assert!(matches!(
            decode_frame::<String>(&invalid_utf8, 64),
            Err(ProtocolError::InvalidJson(_))
        ));
    }

    #[test]
    fn response_limit_includes_only_protocol_headroom() {
        let mut oversized = response();
        oversized.stdout = "x".repeat(RESPONSE_FRAME_HEADROOM_BYTES + 1);
        assert!(matches!(
            encode_response(&oversized, 0),
            Err(ProtocolError::FrameTooLarge { .. })
        ));
        let within_limit = response();
        assert!(encode_response(&within_limit, 0).is_ok());
    }

    #[test]
    fn response_accepts_legacy_cache_state_key() {
        let payload = br#"{
            "version": 1,
            "cache_state": "hit",
            "stdout": "",
            "stderr": "",
            "exit_code": 0,
            "generation": 1
        }"#;
        let response = decode_response_body(payload, 0).expect("decode response");
        assert_eq!(response.status, CacheState::Hit);
        assert_eq!(response.timings, LookupTimings::default());
    }
}
