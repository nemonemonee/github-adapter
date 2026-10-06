#![cfg(target_os = "macos")]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn read_available(terminal: &mut File, transcript: &mut Vec<u8>) -> bool {
    let mut descriptor = libc::pollfd {
        fd: terminal.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut descriptor, 1, 100) } <= 0 {
        return false;
    }
    let mut bytes = [0; 1024];
    if let Ok(read) = terminal.read(&mut bytes) {
        transcript.extend_from_slice(&bytes[..read]);
        assert!(transcript.len() < 64 * 1024);
        return read != 0;
    }
    false
}

#[test]
fn interrupt_and_terminate_restore_hidden_terminal_input_without_echoing_tokens() {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let mut terminal = unsafe { File::from_raw_fd(master_fd) };
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        let mut previous: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut previous) },
            0
        );
        let mut child = OwnedChild(
            Command::new(env!("CARGO_BIN_EXE_github-adapter"))
                .args(["login", "--token"])
                .env_remove("GITHUB_ADAPTER_TOKEN")
                .env_remove("MAI_ADAPTER_GITHUB_TOKEN")
                .env_remove("GITHUB_ADAPTER_CLIENT_ID")
                .env_remove("MAI_ADAPTER_GITHUB_CLIENT_ID")
                .stdin(Stdio::from(slave.try_clone().unwrap()))
                .stdout(Stdio::from(slave.try_clone().unwrap()))
                .stderr(Stdio::from(slave.try_clone().unwrap()))
                .spawn()
                .unwrap(),
        );
        let mut transcript = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !String::from_utf8_lossy(&transcript).contains("input hidden") {
            assert!(
                Instant::now() < deadline,
                "Hidden-input prompt did not appear."
            );
            read_available(&mut terminal, &mut transcript);
        }
        let mut hidden = previous;
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut hidden) },
            0
        );
        assert_eq!(hidden.c_lflag & libc::ECHO, 0);
        terminal
            .write_all(b"fixture-secret-that-must-not-echo")
            .unwrap();
        assert_eq!(
            unsafe { libc::kill(child.0.id() as libc::pid_t, signal) },
            0
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(!status.success());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Hidden input did not stop after a signal."
            );
            read_available(&mut terminal, &mut transcript);
        }
        read_available(&mut terminal, &mut transcript);
        let mut restored = previous;
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut restored) },
            0
        );
        assert_eq!(restored.c_lflag, previous.c_lflag);
        assert!(
            !String::from_utf8_lossy(&transcript).contains("fixture-secret-that-must-not-echo")
        );
    }
}
