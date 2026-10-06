//! Platform credential storage. Windows retains Python keyring 25.6 WinVault compatibility.
//! macOS stores UTF-8 generic passwords in the current user's Keychain.
//!
//! The caller must serialize account mutations: the legacy compound-copy then
//! primary-write sequence is not a transaction. Returned `String`s are owned by
//! the caller and are not guaranteed to be erased from memory.

use adapter_protocol::{AdapterError, Result};

const MAX_TARGET_UNITS: usize = 32_767;
const MAX_USERNAME_UNITS: usize = 513;
const MAX_BLOB_BYTES: usize = 2_560;

pub fn read(service: &str, username: &str) -> Result<Option<String>> {
    validate_arguments(service, username)?;
    native::read(service, username)
}

pub fn write(service: &str, username: &str, secret: &str) -> Result<()> {
    validate_arguments(service, username)?;
    validate_secret(secret)?;
    native::write(service, username, secret)
}

pub fn delete(service: &str, username: &str) -> Result<bool> {
    validate_arguments(service, username)?;
    native::delete(service, username)
}

fn validate_arguments(service: &str, username: &str) -> Result<()> {
    validate_target(service)?;
    if username.contains('\0') || username.encode_utf16().count() > MAX_USERNAME_UNITS {
        return Err(AdapterError::invalid(
            "The credential username contains a NUL or exceeds the Windows length limit.",
        ));
    }
    Ok(())
}

fn validate_target(target: &str) -> Result<()> {
    if target.is_empty()
        || target.contains('\0')
        || target.encode_utf16().count() > MAX_TARGET_UNITS
    {
        return Err(AdapterError::invalid(
            "The credential target must be nonempty, contain no NUL, and fit the Windows length limit.",
        ));
    }
    Ok(())
}

fn validate_secret(secret: &str) -> Result<()> {
    if secret.encode_utf16().count() > MAX_BLOB_BYTES / 2 {
        return Err(AdapterError::invalid(
            "The UTF-16 credential blob exceeds the Windows size limit.",
        ));
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn compound_target(service: &str, username: &str) -> Result<String> {
    let target = format!("{username}@{service}");
    validate_target(&target)?;
    Ok(target)
}

#[cfg(any(windows, test))]
fn format_error() -> AdapterError {
    AdapterError::new(
        502,
        "credential_format_error",
        "Windows Credential Manager returned an invalid credential encoding or structure.",
    )
}

#[cfg(any(windows, test))]
fn decode_blob(blob: &[u8]) -> Result<String> {
    // Python's utf-16 decoder consumes a BOM, otherwise uses native endianness
    // (little endian on Windows), and falls back to UTF-8 only on decoding failure.
    let (big_endian, data) = if let Some(data) = blob.strip_prefix(&[0xfe, 0xff]) {
        (true, data)
    } else {
        (false, blob.strip_prefix(&[0xff, 0xfe]).unwrap_or(blob))
    };
    let (pairs, remainder) = data.as_chunks::<2>();
    if remainder.is_empty() {
        let units = pairs.iter().map(|pair| {
            if big_endian {
                u16::from_be_bytes(*pair)
            } else {
                u16::from_le_bytes(*pair)
            }
        });
        if let Ok(value) = char::decode_utf16(units).collect::<std::result::Result<String, _>>() {
            return Ok(value);
        }
    }
    String::from_utf8(blob.to_vec()).map_err(|_| format_error())
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::ptr::{self, NonNull};
    use std::sync::atomic::{Ordering, compiler_fence};
    use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
    use windows_sys::Win32::Security::Credentials::{
        CRED_PERSIST_ENTERPRISE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW,
        CredWriteW,
    };

    struct Credential(NonNull<CREDENTIALW>);

    impl Credential {
        fn username(&self) -> Result<Option<String>> {
            // CredReadW owns these terminated UTF-16 strings until CredFree.
            let pointer = unsafe { self.0.as_ref().UserName };
            if pointer.is_null() {
                return Ok(None);
            }
            for length in 0..=MAX_USERNAME_UNITS {
                if unsafe { *pointer.add(length) } == 0 {
                    let units = unsafe { std::slice::from_raw_parts(pointer, length) };
                    return String::from_utf16(units)
                        .map(Some)
                        .map_err(|_| format_error());
                }
            }
            Err(format_error())
        }

        fn secret(&self) -> Result<String> {
            let record = unsafe { self.0.as_ref() };
            let size = record.CredentialBlobSize as usize;
            if size > MAX_BLOB_BYTES || (size != 0 && record.CredentialBlob.is_null()) {
                return Err(format_error());
            }
            let blob = if size == 0 {
                &[]
            } else {
                // The returned allocation contains exactly CredentialBlobSize
                // readable bytes; no terminating UTF-16 NUL is part of the blob.
                unsafe { std::slice::from_raw_parts(record.CredentialBlob, size) }
            };
            decode_blob(blob)
        }
    }

    impl Drop for Credential {
        fn drop(&mut self) {
            // CredReadW transfers an allocation to the caller. Wipe its blob
            // while valid, then free the entire allocation exactly once.
            unsafe {
                let record = self.0.as_ref();
                if !record.CredentialBlob.is_null() {
                    wipe(record.CredentialBlob, record.CredentialBlobSize as usize);
                }
                CredFree(self.0.as_ptr().cast());
            }
        }
    }

    struct SecretBytes(Vec<u8>);

    impl Drop for SecretBytes {
        fn drop(&mut self) {
            unsafe { wipe(self.0.as_mut_ptr(), self.0.len()) };
        }
    }

    struct SecretText(String);

    impl Drop for SecretText {
        fn drop(&mut self) {
            // Replacing each byte with NUL preserves UTF-8 validity. This only
            // clears this allocation, not other copies or the caller's input.
            unsafe {
                let bytes = self.0.as_bytes_mut();
                wipe(bytes.as_mut_ptr(), bytes.len());
            }
        }
    }

    unsafe fn wipe(pointer: *mut u8, length: usize) {
        for offset in 0..length {
            unsafe { ptr::write_volatile(pointer.add(offset), 0) };
        }
        compiler_fence(Ordering::SeqCst);
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn store_error(operation: &str, code: u32) -> AdapterError {
        AdapterError::new(
            502,
            "credential_store_error",
            format!(
                "Windows Credential Manager could not {operation} a credential (Win32 error {code}). No plaintext fallback is used."
            ),
        )
    }

    fn read_target(target: &str) -> Result<Option<Credential>> {
        validate_target(target)?;
        let target = wide(target);
        let mut pointer = ptr::null_mut();
        let succeeded = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut pointer) };
        if succeeded == 0 {
            let code = unsafe { GetLastError() };
            return if code == ERROR_NOT_FOUND {
                Ok(None)
            } else {
                Err(store_error("read", code))
            };
        }
        NonNull::new(pointer)
            .map(Credential)
            .map(Some)
            .ok_or_else(format_error)
    }

    fn write_target(target: &str, username: Option<&str>, secret: &str) -> Result<()> {
        validate_arguments(target, username.unwrap_or(""))?;
        validate_secret(secret)?;
        let mut target = wide(target);
        let mut username = username.map(wide);
        let mut comment = wide("Stored using GitHub Adapter");
        let mut blob = SecretBytes(secret.encode_utf16().flat_map(u16::to_le_bytes).collect());
        // Every field is a pointer or integer (including FILETIME); unused
        // attributes, aliases, flags and timestamps must be zero.
        let mut credential: CREDENTIALW = unsafe { std::mem::zeroed() };
        credential.Type = CRED_TYPE_GENERIC;
        credential.TargetName = target.as_mut_ptr();
        credential.UserName = username
            .as_mut()
            .map_or(ptr::null_mut(), |value| value.as_mut_ptr());
        credential.Comment = comment.as_mut_ptr();
        credential.CredentialBlobSize = blob.0.len() as u32;
        credential.CredentialBlob = blob.0.as_mut_ptr();
        credential.Persist = CRED_PERSIST_ENTERPRISE;
        // All pointed-to buffers remain alive throughout this synchronous call.
        if unsafe { CredWriteW(&credential, 0) } == 0 {
            return Err(store_error("write", unsafe { GetLastError() }));
        }
        Ok(())
    }

    fn delete_target(target: &str) -> Result<()> {
        let target = wide(target);
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
            let code = unsafe { GetLastError() };
            if code != ERROR_NOT_FOUND {
                return Err(store_error("delete", code));
            }
        }
        Ok(())
    }

    pub(super) fn read(service: &str, username: &str) -> Result<Option<String>> {
        if let Some(primary) = read_target(service)?
            && (username.is_empty() || primary.username()?.as_deref() == Some(username))
        {
            return primary.secret().map(Some);
        }
        // WinVault does not re-check UserName on a compound-target fallback.
        read_target(&compound_target(service, username)?)?
            .map(|credential| credential.secret())
            .transpose()
    }

    pub(super) fn write(service: &str, username: &str, secret: &str) -> Result<()> {
        if let Some(primary) = read_target(service)? {
            let previous_username = primary.username()?;
            let previous = SecretText(primary.secret()?);
            // Python copies the old primary even when the username is unchanged.
            // Its f-string renders an absent native UserName as "None".
            let compound =
                compound_target(service, previous_username.as_deref().unwrap_or("None"))?;
            write_target(&compound, previous_username.as_deref(), &previous.0)?;
        }
        write_target(service, Some(username), secret)
    }

    pub(super) fn delete(service: &str, username: &str) -> Result<bool> {
        let compound = compound_target(service, username)?;
        let mut deleted = false;
        for target in [service, compound.as_str()] {
            if let Some(credential) = read_target(target)?
                && credential.username()?.as_deref() == Some(username)
            {
                delete_target(target)?;
                deleted = true;
            }
        }
        Ok(deleted)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn native_errors_disclose_only_operation_and_numeric_status() {
            let error = store_error("read", 5);
            assert_eq!(error.status, 502);
            assert_eq!(error.code, "credential_store_error");
            assert!(error.message.contains("Win32 error 5"));
            assert!(!error.message.contains("MAI Adapter"));
        }
    }
}

#[cfg(target_os = "macos")]
#[path = "macos_credentials.rs"]
mod native;

#[cfg(not(any(windows, target_os = "macos")))]
mod native {
    use super::*;

    fn unsupported() -> AdapterError {
        AdapterError::new(
            501,
            "unsupported_platform",
            "Native credential storage requires Windows or macOS.",
        )
    }

    pub(super) fn read(_: &str, _: &str) -> Result<Option<String>> {
        Err(unsupported())
    }

    pub(super) fn write(_: &str, _: &str, _: &str) -> Result<()> {
        Err(unsupported())
    }

    pub(super) fn delete(_: &str, _: &str) -> Result<bool> {
        Err(unsupported())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_compound_names_preserve_case_and_punctuation() {
        assert_eq!(
            compound_target("MAI Adapter", "personal-github").unwrap(),
            "personal-github@MAI Adapter"
        );
        assert_eq!(compound_target("service", "").unwrap(), "@service");
        assert!(compound_target(&"a".repeat(MAX_TARGET_UNITS), "u").is_err());
    }

    #[test]
    fn blob_decode_matches_windows_python_utf16_then_utf8() {
        let value = "fixture-\u{96ea}-\u{1f680}\0end";
        let little: Vec<u8> = value.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let big: Vec<u8> = value.encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert!(decode_blob(&little).is_ok_and(|decoded| decoded == value));
        assert!(
            decode_blob(&[&[0xff, 0xfe][..], &little].concat())
                .is_ok_and(|decoded| decoded == value)
        );
        assert!(
            decode_blob(&[&[0xfe, 0xff][..], &big].concat()).is_ok_and(|decoded| decoded == value)
        );
        assert!(decode_blob(b"odd").is_ok_and(|decoded| decoded == "odd"));
        // Even-length UTF-8 that is valid UTF-16 is *not* treated as UTF-8.
        assert!(decode_blob(b"ab").is_ok_and(|decoded| decoded == "\u{6261}"));
        assert!(decode_blob(&[]).is_ok_and(|decoded| decoded.is_empty()));
        for invalid in [&[0xff][..], &[0x00, 0xd8], &[0xfe, 0xff, 0xd8, 0x00]] {
            let error = decode_blob(invalid).unwrap_err();
            assert_eq!(error.code, "credential_format_error");
            assert_eq!(error.message, format_error().message);
        }
    }

    #[test]
    fn utf16_limits_count_surrogate_pairs_and_allow_empty_or_nul_secrets() {
        assert!(validate_secret("").is_ok());
        assert!(validate_secret("fixture\0value").is_ok());
        assert!(validate_secret(&"a".repeat(MAX_BLOB_BYTES / 2)).is_ok());
        assert!(validate_secret(&"a".repeat(MAX_BLOB_BYTES / 2 + 1)).is_err());
        assert!(validate_secret(&"\u{1f680}".repeat(MAX_BLOB_BYTES / 4)).is_ok());
        assert!(validate_secret(&"\u{1f680}".repeat(MAX_BLOB_BYTES / 4 + 1)).is_err());
        assert!(validate_arguments("service", "").is_ok());
        assert!(validate_arguments(" service ", " User ").is_ok());
    }
}
