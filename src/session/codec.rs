use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use thiserror::Error;

use crate::arena::{Checkpoint, RuntimeIdentity};
use crate::tools::{ContentFingerprint, WholeFileAuthorisations};

#[derive(Debug, Error)]
pub(super) enum CodecError {
    #[error("stored session JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("stored session path is not hexadecimal")]
    InvalidPath,
    #[error("stored content fingerprint is not a 32-byte hexadecimal value")]
    InvalidFingerprint,
    #[cfg(not(unix))]
    #[error("session paths are supported only on Unix")]
    UnsupportedPath,
}

#[derive(Serialize, Deserialize)]
struct StoredAuthorisation {
    path: String,
    fingerprint: String,
}

pub(super) struct StoredCheckpoint {
    pub(super) runtime_identity: String,
    pub(super) arena_state: Vec<u8>,
    pub(super) authorisations_json: String,
}

pub(super) fn encode_checkpoint(checkpoint: &Checkpoint) -> Result<StoredCheckpoint, CodecError> {
    let (identity, state, authorisations) = checkpoint.session_parts();
    let authorisations = authorisations
        .session_entries()
        .map(|(path, fingerprint)| {
            Ok(StoredAuthorisation {
                path: encode_path(path)?,
                fingerprint: encode_hex(&fingerprint.session_bytes()),
            })
        })
        .collect::<Result<Vec<_>, CodecError>>()?;
    Ok(StoredCheckpoint {
        runtime_identity: identity.session_value().to_owned(),
        arena_state: state.to_vec(),
        authorisations_json: serde_json::to_string(&authorisations)?,
    })
}

pub(super) fn decode_checkpoint(
    runtime_identity: String,
    arena_state: Vec<u8>,
    authorisations_json: &str,
) -> Result<Checkpoint, CodecError> {
    let records: Vec<StoredAuthorisation> = serde_json::from_str(authorisations_json)?;
    let mut authorisations = WholeFileAuthorisations::default();
    for record in records {
        let path = decode_path(&record.path)?;
        let fingerprint = decode_fingerprint(&record.fingerprint)?;
        authorisations.authorise(path, fingerprint);
    }
    Ok(Checkpoint::from_session_parts(
        RuntimeIdentity::from_session_value(runtime_identity),
        arena_state,
        authorisations,
    ))
}

pub(super) fn encode_path(path: &Path) -> Result<String, CodecError> {
    #[cfg(unix)]
    {
        Ok(encode_hex(path.as_os_str().as_bytes()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(CodecError::UnsupportedPath)
    }
}

pub(super) fn decode_path(value: &str) -> Result<PathBuf, CodecError> {
    #[cfg(unix)]
    {
        Ok(PathBuf::from(std::ffi::OsString::from_vec(decode_hex(
            value,
        )?)))
    }
    #[cfg(not(unix))]
    {
        let _ = value;
        Err(CodecError::UnsupportedPath)
    }
}

fn decode_fingerprint(value: &str) -> Result<ContentFingerprint, CodecError> {
    let bytes = decode_hex(value).map_err(|_| CodecError::InvalidFingerprint)?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| CodecError::InvalidFingerprint)?;
    Ok(ContentFingerprint::from_session_bytes(bytes))
}

pub(super) fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[usize::from(byte >> 4)] as char);
        encoded.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn decode_hex(value: &str) -> Result<Vec<u8>, CodecError> {
    if value.len() % 2 != 0 {
        return Err(CodecError::InvalidPath);
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Ok((hex_value(pair[0])? << 4) | hex_value(pair[1])?))
        .collect()
}

fn hex_value(value: u8) -> Result<u8, CodecError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(CodecError::InvalidPath),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    use super::{decode_checkpoint, decode_path, encode_checkpoint, encode_path};
    use crate::arena::{Checkpoint, RuntimeIdentity};
    use crate::tools::{ContentFingerprint, WholeFileAuthorisations};

    #[test]
    #[cfg(unix)]
    fn checkpoint_codec_preserves_non_utf8_authorisations() {
        let path = PathBuf::from(OsString::from_vec(b"nested/\xff-file".to_vec()));
        let fingerprint = ContentFingerprint::from_session_bytes([42; 32]);
        let mut authorisations = WholeFileAuthorisations::default();
        authorisations.authorise(path.clone(), fingerprint);
        let checkpoint = Checkpoint::from_session_parts(
            RuntimeIdentity::from_session_value("monty-runtime 0.0.22".to_owned()),
            vec![0, 1, 2, 255],
            authorisations,
        );

        let encoded = encode_checkpoint(&checkpoint).expect("encode checkpoint");
        let decoded = decode_checkpoint(
            encoded.runtime_identity,
            encoded.arena_state,
            &encoded.authorisations_json,
        )
        .expect("decode checkpoint");

        assert_eq!(decoded, checkpoint);
        assert_eq!(decode_path(&encode_path(&path).unwrap()).unwrap(), path);
    }
}
