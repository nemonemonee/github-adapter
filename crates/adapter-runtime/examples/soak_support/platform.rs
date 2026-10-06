use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Read;

use crate::Check;

pub const PRIVATE_BYTES_LIMIT: u64 = 512 * 1024 * 1024;
pub const HANDLES_LIMIT: u64 = 4096;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResourceSample {
    pub active_elapsed_ms: u64,
    pub private_bytes: u64,
    pub working_set_bytes: u64,
    pub handles: u64,
    pub cpu_ms: u64,
}

pub fn identity() -> Check<Value> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut file = std::fs::File::open(&executable).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if metadata.len() > 256 * 1024 * 1024 {
        return Err("Example artifact exceeds its fingerprint read limit.".into());
    }
    // A local streaming fingerprint supplements the launcher's cryptographic artifact SHA-256.
    let mut hash = 0xcbf29ce484222325_u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        for byte in &buffer[..count] {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
    }
    let build = option_env!("ADAPTER_SOAK_BUILD_IDENTITY")
        .filter(|text| text.len() <= 8192)
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    Ok(json!({
        "package_version": env!("CARGO_PKG_VERSION"),
        "debug_assertions": cfg!(debug_assertions),
        "process_architecture": std::env::consts::ARCH,
        "pointer_bits": usize::BITS,
        "platform": platform_identity()?,
        "executable": executable,
        "artifact_bytes": metadata.len(),
        "artifact_fingerprint": {"algorithm": "fnv1a64", "value": format!("{hash:016x}")},
        "resource_limits": {"private_bytes": PRIVATE_BYTES_LIMIT, "handles": HANDLES_LIMIT},
        "build": build,
        "scope": "synthetic loopback MAI Backend + HTTP only; no real clients, credentials, TLS or remote inference",
    }))
}

#[cfg(windows)]
mod windows {
    #[repr(C)]
    #[derive(Default)]
    pub struct Memory {
        pub size: u32,
        pub faults: u32,
        pub peak_working_set: usize,
        pub working_set: usize,
        pub peak_paged_pool: usize,
        pub paged_pool: usize,
        pub peak_nonpaged_pool: usize,
        pub nonpaged_pool: usize,
        pub pagefile: usize,
        pub peak_pagefile: usize,
        pub private_usage: usize,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct FileTime {
        pub low: u32,
        pub high: u32,
    }

    impl FileTime {
        pub fn ticks(&self) -> u64 {
            (u64::from(self.high) << 32) | u64::from(self.low)
        }
    }

    #[repr(C)]
    pub struct Version {
        pub size: u32,
        pub major: u32,
        pub minor: u32,
        pub build: u32,
        pub platform: u32,
        pub service_pack: [u16; 128],
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetCurrentProcess() -> isize;
        pub fn K32GetProcessMemoryInfo(process: isize, memory: *mut Memory, size: u32) -> i32;
        pub fn GetProcessHandleCount(process: isize, count: *mut u32) -> i32;
        pub fn GetProcessTimes(
            process: isize,
            created: *mut FileTime,
            exited: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        pub fn IsWow64Process2(
            process: isize,
            process_machine: *mut u16,
            native_machine: *mut u16,
        ) -> i32;
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        pub fn RtlGetVersion(version: *mut Version) -> i32;
    }
}

#[cfg(windows)]
fn platform_identity() -> Check<Value> {
    use windows::*;
    let mut process_machine = 0;
    let mut native_machine = 0;
    let mut version = Version {
        size: std::mem::size_of::<Version>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    // All buffers have the Win32 ABI layout and remain live for these synchronous calls.
    let ok = unsafe {
        IsWow64Process2(
            GetCurrentProcess(),
            &mut process_machine,
            &mut native_machine,
        ) != 0
            && RtlGetVersion(&mut version) == 0
    };
    if !ok {
        return Err("Cannot identify the native Windows OS/process architecture.".into());
    }
    let architecture = match native_machine {
        0xaa64 => "aarch64",
        0x8664 => "x86_64",
        _ => "unsupported",
    };
    Ok(json!({
        "os": "windows",
        "version": format!("{}.{}.{}", version.major, version.minor, version.build),
        "native_architecture": architecture,
        "native_machine": native_machine,
        "process_machine": process_machine,
        "emulated": process_machine != 0 && process_machine != native_machine,
    }))
}

#[cfg(windows)]
pub fn sample(active_elapsed_ms: u64) -> Check<ResourceSample> {
    use windows::*;
    let size = std::mem::size_of::<Memory>() as u32;
    let mut memory = Memory {
        size,
        ..Memory::default()
    };
    let mut handles = 0;
    let (mut created, mut exited, mut kernel, mut user) = (
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
    );
    // The pseudo-handle is only for this native process; no other process is inspected.
    let ok = unsafe {
        let process = GetCurrentProcess();
        K32GetProcessMemoryInfo(process, &mut memory, size) != 0
            && GetProcessHandleCount(process, &mut handles) != 0
            && GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) != 0
    };
    if !ok {
        return Err("Native process resource sampling failed.".into());
    }
    Ok(ResourceSample {
        active_elapsed_ms,
        private_bytes: memory.private_usage as u64,
        working_set_bytes: memory.working_set as u64,
        handles: u64::from(handles),
        cpu_ms: (kernel.ticks() + user.ticks()) / 10_000,
    })
}

#[cfg(not(windows))]
fn platform_identity() -> Check<Value> {
    Ok(json!({"os": std::env::consts::OS, "native_architecture": null, "emulated": null}))
}

#[cfg(not(windows))]
pub fn sample(_active_elapsed_ms: u64) -> Check<ResourceSample> {
    Err(
        "The native resource sampler requires Windows; no resource qualification is claimed."
            .into(),
    )
}
