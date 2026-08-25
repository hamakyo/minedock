//! Local-network discovery for the connection endpoint shown by the UI.
//!
//! The Windows API call is kept in this app-side adapter. The filtering and
//! ambiguity rules are platform-independent so they can be tested with
//! deterministic fixtures without depending on the host's interfaces.

use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanIpv4Candidate {
    pub address: Ipv4Addr,
    pub interface_name: String,
}

impl LanIpv4Candidate {
    pub fn new(address: Ipv4Addr, interface_name: impl Into<String>) -> Self {
        let interface_name = interface_name.into();
        Self {
            address,
            interface_name: if interface_name.trim().is_empty() {
                "Unknown network adapter".into()
            } else {
                interface_name
            },
        }
    }

    pub fn endpoint(&self, port: u16) -> Option<String> {
        (port != 0).then(|| format!("{}:{port}", self.address))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LanAddressState {
    #[default]
    Checking,
    Unavailable(LanAddressUnavailableReason),
    Available(LanIpv4Candidate),
    Ambiguous(Vec<LanIpv4Candidate>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanAddressUnavailableReason {
    NoUsablePrivateIpv4,
    AdapterEnumerationFailed(String),
}

pub fn discover_lan_address_state() -> LanAddressState {
    match discover_system_candidates() {
        Ok(candidates) => select_lan_address(candidates),
        Err(error) => LanAddressState::Unavailable(
            LanAddressUnavailableReason::AdapterEnumerationFailed(error),
        ),
    }
}

pub fn select_lan_address(
    candidates: impl IntoIterator<Item = LanIpv4Candidate>,
) -> LanAddressState {
    let mut candidates: Vec<_> = candidates
        .into_iter()
        .filter(|candidate| is_usable_lan_ipv4(candidate.address))
        .collect();
    candidates.sort_by(|left, right| {
        left.address
            .octets()
            .cmp(&right.address.octets())
            .then_with(|| left.interface_name.cmp(&right.interface_name))
    });
    candidates.dedup();

    match candidates.len() {
        0 => LanAddressState::Unavailable(LanAddressUnavailableReason::NoUsablePrivateIpv4),
        1 => LanAddressState::Available(candidates.remove(0)),
        _ => LanAddressState::Ambiguous(candidates),
    }
}

fn is_usable_lan_ipv4(address: Ipv4Addr) -> bool {
    let [first, second, _, _] = address.octets();
    (first == 10)
        || (first == 172 && (16..=31).contains(&second))
        || (first == 192 && second == 168)
}

#[cfg(windows)]
fn discover_system_candidates() -> Result<Vec<LanIpv4Candidate>, String> {
    use std::{mem::MaybeUninit, ptr::null};

    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET;

    let mut buffer_size = 15 * 1024u32;
    loop {
        let word_count = (buffer_size as usize).div_ceil(std::mem::size_of::<u64>());
        let mut buffer: Vec<MaybeUninit<u64>> = Vec::with_capacity(word_count);
        // GetAdaptersAddresses writes initialized adapter records into this
        // aligned byte buffer. The list remains valid while `buffer` lives.
        unsafe { buffer.set_len(word_count) };
        let result = unsafe {
            GetAdaptersAddresses(
                AF_INET as u32,
                GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                null(),
                buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
                &mut buffer_size,
            )
        };
        if result == ERROR_BUFFER_OVERFLOW {
            continue;
        }
        if result != ERROR_SUCCESS {
            return Err(format!(
                "Windows network adapter discovery failed (error {result})"
            ));
        }

        return Ok(unsafe { parse_adapter_list(buffer.as_ptr().cast()) });
    }
}

#[cfg(not(windows))]
fn discover_system_candidates() -> Result<Vec<LanIpv4Candidate>, String> {
    Err("LAN address discovery is unavailable on this platform".into())
}

#[cfg(windows)]
unsafe fn parse_adapter_list(
    mut adapter: *const windows_sys::Win32::NetworkManagement::IpHelper::IP_ADAPTER_ADDRESSES_LH,
) -> Vec<LanIpv4Candidate> {
    use std::mem::size_of;

    use windows_sys::Win32::NetworkManagement::IpHelper::IP_ADAPTER_UNICAST_ADDRESS_LH;
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;

    let mut candidates = Vec::new();
    while !adapter.is_null() {
        let adapter_ref = unsafe { &*adapter };
        if adapter_ref.OperStatus == IfOperStatusUp {
            let interface_name = unsafe { windows_wide_string(adapter_ref.FriendlyName) };
            let mut unicast = adapter_ref.FirstUnicastAddress;
            while !unicast.is_null() {
                let unicast_ref: &IP_ADAPTER_UNICAST_ADDRESS_LH = unsafe { &*unicast };
                if let Some(address) = unsafe {
                    ipv4_from_socket_address(
                        &unicast_ref.Address,
                        size_of::<windows_sys::Win32::Networking::WinSock::SOCKADDR_IN>(),
                    )
                } {
                    candidates.push(LanIpv4Candidate::new(address, &interface_name));
                }
                unicast = unicast_ref.Next;
            }
        }
        adapter = adapter_ref.Next;
    }
    candidates
}

#[cfg(windows)]
unsafe fn ipv4_from_socket_address(
    address: &windows_sys::Win32::Networking::WinSock::SOCKET_ADDRESS,
    minimum_length: usize,
) -> Option<Ipv4Addr> {
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

    if address.lpSockaddr.is_null() || (address.iSockaddrLength as usize) < minimum_length {
        return None;
    }
    let sockaddr = unsafe { &*address.lpSockaddr.cast::<SOCKADDR_IN>() };
    if sockaddr.sin_family != AF_INET {
        return None;
    }
    let bytes = unsafe { sockaddr.sin_addr.S_un.S_un_b };
    Some(Ipv4Addr::new(
        bytes.s_b1, bytes.s_b2, bytes.s_b3, bytes.s_b4,
    ))
}

#[cfg(windows)]
unsafe fn windows_wide_string(pointer: *const u16) -> String {
    if pointer.is_null() {
        return "Unknown network adapter".into();
    }
    let mut length = 0;
    while length < 256 && unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(pointer, length) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(address: &str, interface_name: &str) -> LanIpv4Candidate {
        LanIpv4Candidate::new(address.parse().expect("fixture address"), interface_name)
    }

    #[test]
    fn endpoint_uses_the_configured_server_port() {
        let candidate = candidate("192.168.1.20", "Wi-Fi");
        assert_eq!(
            candidate.endpoint(25570).as_deref(),
            Some("192.168.1.20:25570")
        );
        assert_eq!(candidate.endpoint(0), None);
    }

    #[test]
    fn loopback_link_local_and_public_addresses_are_not_lan_candidates() {
        let state = select_lan_address([
            candidate("127.0.0.1", "Loopback"),
            candidate("169.254.10.20", "Link-local"),
            candidate("8.8.8.8", "Public"),
        ]);
        assert!(matches!(state, LanAddressState::Unavailable(_)));
    }

    #[test]
    fn one_private_address_is_selected() {
        let state = select_lan_address([candidate("192.168.1.20", "Wi-Fi")]);
        assert_eq!(
            state,
            LanAddressState::Available(candidate("192.168.1.20", "Wi-Fi"))
        );
    }

    #[test]
    fn multiple_private_addresses_are_explicitly_ambiguous() {
        let state = select_lan_address([
            candidate("172.24.10.5", "vEthernet (WSL)"),
            candidate("192.168.1.20", "Wi-Fi"),
            candidate("172.18.0.1", "DockerNAT"),
        ]);
        assert!(matches!(state, LanAddressState::Ambiguous(candidates) if candidates.len() == 3));
    }

    #[test]
    fn duplicate_candidates_are_removed_deterministically() {
        let state = select_lan_address([
            candidate("192.168.1.20", "Wi-Fi"),
            candidate("192.168.1.20", "Wi-Fi"),
        ]);
        assert!(matches!(state, LanAddressState::Available(_)));
    }
}
