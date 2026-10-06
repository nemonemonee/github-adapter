use super::*;
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};

const ITEM_NOT_FOUND: i32 = -25_300;

fn store_error(operation: &str, status: i32) -> AdapterError {
    AdapterError::new(
        502,
        "credential_store_error",
        format!(
            "macOS Keychain could not {operation} a credential (OSStatus {status}). No plaintext fallback is used."
        ),
    )
}

pub(super) fn read(service: &str, username: &str) -> Result<Option<String>> {
    match get_generic_password(service, username) {
        Ok(mut bytes) => {
            let result = std::str::from_utf8(&bytes).map(str::to_owned).map_err(|_| {
                AdapterError::new(
                    502,
                    "credential_format_error",
                    "macOS Keychain returned invalid credential text.",
                )
            });
            // Wipe the native return buffer; the returned String belongs to the caller.
            for byte in &mut bytes {
                unsafe { std::ptr::write_volatile(byte, 0) };
            }
            std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
            result.map(Some)
        }
        Err(error) if error.code() == ITEM_NOT_FOUND => Ok(None),
        Err(error) => Err(store_error("read", error.code())),
    }
}

pub(super) fn write(service: &str, username: &str, secret: &str) -> Result<()> {
    set_generic_password(service, username, secret.as_bytes())
        .map_err(|error| store_error("write", error.code()))
}

pub(super) fn delete(service: &str, username: &str) -> Result<bool> {
    match delete_generic_password(service, username) {
        Ok(()) => Ok(true),
        Err(error) if error.code() == ITEM_NOT_FOUND => Ok(false),
        Err(error) => Err(store_error("delete", error.code())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_errors_expose_only_operation_and_numeric_status() {
        let error = store_error("read", -25_308);
        assert_eq!(error.code, "credential_store_error");
        assert!(error.message.contains("OSStatus -25308"));
        assert!(!error.message.contains("MAI Adapter"));
    }
}
