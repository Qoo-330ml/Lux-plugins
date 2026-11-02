//! Address classification shared by the notifier SDK destination guard.
//!
//! Mirrors the Lux core webhook classification so plugins refuse the same dangerous destinations.

use std::net::IpAddr;

use super::remote_url_policy;

/// Whether an address must never be a webhook destination, even when the administrator approved the
/// private network: unspecified, link-local (cloud metadata), multicast and broadcast ranges.
///
/// Shared with the notifier plugin SDK destination guard so both keep one classification.
pub fn is_dangerous_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(value) => {
            value.is_unspecified()
                || value.is_link_local()
                || value.is_multicast()
                || value.octets()[0] == 0
        }
        IpAddr::V6(value) => {
            if let Some(mapped) = value.to_ipv4_mapped() {
                return is_dangerous_address(IpAddr::V4(mapped));
            }
            // A deprecated IPv4-compatible address (`::/96`) still carries an IPv4 target, so it
            // must not bypass the IPv4 policy; `::` and `::1` keep their IPv6 meaning.
            if let Some(compatible) = remote_url_policy::ipv4_compatible_target(value) {
                return is_dangerous_address(IpAddr::V4(compatible));
            }
            value.is_unspecified() || value.is_multicast() || value.is_unicast_link_local()
        }
    }
}

/// Whether an address is a loopback, private or link-local destination that the host only permits
/// after an explicit private-network approval.
///
/// Shared with the notifier plugin SDK destination guard so both keep one classification.
pub fn is_private_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(value) => {
            value.is_private()
                || value.is_loopback()
                || value.is_link_local()
                || value.octets()[0] == 100 && (value.octets()[1] & 0b1100_0000) == 0b0100_0000
                || value.octets()[0] == 198 && (value.octets()[1] == 18 || value.octets()[1] == 19)
                || value.octets()[0] == 192 && value.octets()[1] == 0 && value.octets()[2] == 0
        }
        IpAddr::V6(value) => {
            if let Some(mapped) = value.to_ipv4_mapped() {
                return is_private_address(IpAddr::V4(mapped));
            }
            if let Some(compatible) = remote_url_policy::ipv4_compatible_target(value) {
                return is_private_address(IpAddr::V4(compatible));
            }
            let octets = value.octets();
            value.is_loopback() || value.is_unicast_link_local() || (octets[0] & 0xfe) == 0xfc
        }
    }
}
