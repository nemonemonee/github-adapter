use adapter_protocol::{AdapterError, Result};
use sha2::{Digest, Sha256};
use std::ptr;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAceEx, GetLengthSid,
    GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor, SE_DACL_PROTECTED,
    SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorControl,
    SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenSessionId, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

pub(crate) struct UserSecurity {
    descriptor: Box<SECURITY_DESCRIPTOR>,
    _acl: Vec<u32>,
    identity: Identity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Identity {
    pub(crate) user: String,
    pub(crate) session: u32,
}

struct Token {
    handle: Handle,
    user: Vec<usize>,
}

impl Token {
    fn open(process: HANDLE) -> Result<Self> {
        let mut handle = ptr::null_mut();
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut handle) } == 0 {
            return Err(failed());
        }
        let handle = Handle(handle);
        let mut length = 0;
        unsafe { GetTokenInformation(handle.0, TokenUser, ptr::null_mut(), 0, &mut length) };
        if length < std::mem::size_of::<TOKEN_USER>() as u32 || length > 64 * 1024 {
            return Err(failed());
        }
        let mut user = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                handle.0,
                TokenUser,
                user.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } == 0
        {
            return Err(failed());
        }
        Ok(Self { handle, user })
    }

    fn sid(&self) -> *mut std::ffi::c_void {
        unsafe { (*(self.user.as_ptr().cast::<TOKEN_USER>())).User.Sid }
    }

    fn identity(&self) -> Result<Identity> {
        let sid = self.sid();
        let length = unsafe { GetLengthSid(sid) } as usize;
        if length == 0 || length > 1024 {
            return Err(failed());
        }
        let user = format!(
            "{:x}",
            Sha256::digest(unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), length) })
        );
        let mut session = 0u32;
        let mut returned = 0;
        if unsafe {
            GetTokenInformation(
                self.handle.0,
                TokenSessionId,
                (&mut session as *mut u32).cast(),
                std::mem::size_of::<u32>() as u32,
                &mut returned,
            )
        } == 0
            || returned != std::mem::size_of::<u32>() as u32
        {
            return Err(failed());
        }
        Ok(Identity { user, session })
    }
}

pub(crate) fn process_identity(process: HANDLE) -> Result<Identity> {
    Token::open(process)?.identity()
}

impl UserSecurity {
    pub(crate) fn new() -> Result<Self> {
        let token = Token::open(unsafe { GetCurrentProcess() })?;
        // TOKEN_USER and SID point inside this aligned, live native buffer.
        let sid = token.sid();
        let sid_length = unsafe { GetLengthSid(sid) } as usize;
        if sid_length == 0 || sid_length > 1024 {
            return Err(failed());
        }
        let identity = token.identity()?;
        let acl_length =
            std::mem::size_of::<ACL>() + std::mem::size_of::<ACCESS_ALLOWED_ACE>() + sid_length - 4;
        let mut acl = vec![0u32; acl_length.div_ceil(4)];
        let acl_pointer = acl.as_mut_ptr().cast::<ACL>();
        let mut descriptor: Box<SECURITY_DESCRIPTOR> = Box::new(unsafe { std::mem::zeroed() });
        // The descriptor contains one current-user ACE and does not inherit a public DACL.
        let ok = unsafe {
            InitializeAcl(acl_pointer, acl_length as u32, ACL_REVISION) != 0
                && AddAccessAllowedAceEx(acl_pointer, ACL_REVISION, 0, 0x1000_0000, sid) != 0
                && InitializeSecurityDescriptor(
                    (&mut *descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                    1,
                ) != 0
                && SetSecurityDescriptorDacl(
                    (&mut *descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                    1,
                    acl_pointer,
                    0,
                ) != 0
                && SetSecurityDescriptorControl(
                    (&mut *descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                    SE_DACL_PROTECTED,
                    SE_DACL_PROTECTED,
                ) != 0
        };
        if !ok {
            return Err(failed());
        }
        Ok(Self {
            descriptor,
            _acl: acl,
            identity,
        })
    }

    pub(crate) fn identity(&self) -> &str {
        &self.identity.user
    }

    pub(crate) fn scope(&self) -> &Identity {
        &self.identity
    }

    pub(crate) fn attributes(&mut self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: (&mut *self.descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            bInheritHandle: 0,
        }
    }
}

fn failed() -> AdapterError {
    AdapterError::new(
        500,
        "user_security_unavailable",
        format!(
            "Cannot establish current-user control permissions (OS code {}).",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        ),
    )
}
