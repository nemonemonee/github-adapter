use adapter_protocol::{AdapterError, Result};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{SocketAddr, TcpListener};

pub fn bind_loopback(address: SocketAddr) -> Result<TcpListener> {
    if !address.ip().is_loopback() {
        return Err(AdapterError::invalid(
            "The adapter listener must be loopback-only.",
        ));
    }
    let domain = if address.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))
        .map_err(|error| socket_error("create", error))?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            SO_EXCLUSIVEADDRUSE, SOCKET_ERROR, SOL_SOCKET, WSAGetLastError, setsockopt,
        };
        let exclusive: i32 = 1;
        // The initialized socket owns this handle, and the option buffer lives through the call.
        let result = unsafe {
            setsockopt(
                socket.as_raw_socket() as usize,
                SOL_SOCKET,
                SO_EXCLUSIVEADDRUSE,
                (&exclusive as *const i32).cast(),
                std::mem::size_of_val(&exclusive) as i32,
            )
        };
        if result == SOCKET_ERROR {
            let code = unsafe { WSAGetLastError() };
            return Err(socket_error(
                "set exclusive ownership",
                std::io::Error::from_raw_os_error(code),
            ));
        }
    }
    if address.is_ipv6() {
        socket
            .set_only_v6(true)
            .map_err(|error| socket_error("configure IPv6", error))?;
    }
    socket
        .bind(&address.into())
        .map_err(|error| socket_error("bind", error))?;
    socket
        .listen(128)
        .map_err(|error| socket_error("listen", error))?;
    socket
        .set_nonblocking(true)
        .map_err(|error| socket_error("configure nonblocking I/O", error))?;
    Ok(socket.into())
}

fn socket_error(operation: &str, error: std::io::Error) -> AdapterError {
    let detail = match error.kind() {
        std::io::ErrorKind::AddrInUse => "the endpoint is already owned by another listener",
        std::io::ErrorKind::PermissionDenied => "the endpoint is reserved or access is denied",
        _ => "Windows could not acquire the endpoint",
    };
    AdapterError::new(
        503,
        "listener_unavailable",
        format!(
            "Cannot {operation} the adapter socket: {detail} (OS code {}). No process was stopped.",
            error
                .raw_os_error()
                .map_or_else(|| "unavailable".into(), |code| code.to_string())
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn binding_not_a_health_probe_decides_whether_a_free_port_can_start() {
        let first = bind_loopback(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).unwrap();
        let address = first.local_addr().unwrap();
        assert!(address.port() > 0);
        assert_eq!(
            bind_loopback(address).unwrap_err().code,
            "listener_unavailable"
        );
        drop(first);
        assert!(bind_loopback(address).is_ok());
    }

    #[test]
    fn non_loopback_addresses_are_rejected_before_binding() {
        assert_eq!(
            bind_loopback("0.0.0.0:0".parse().unwrap())
                .unwrap_err()
                .status,
            400
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_reuse_address_socket_cannot_override_the_owned_windows_listener() {
        let first = bind_loopback("127.0.0.1:0".parse().unwrap()).unwrap();
        let contender = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        contender.set_reuse_address(true).unwrap();
        assert!(contender.bind(&first.local_addr().unwrap().into()).is_err());
    }
}
