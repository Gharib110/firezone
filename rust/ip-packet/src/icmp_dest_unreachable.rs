use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{Context as _, Result};
use etherparse::{Icmpv4Type, Icmpv6Type, LaxIpv4Slice, LaxIpv6Slice, icmpv4, icmpv6};

use crate::{Layer4Protocol, Protocol, nat46, nat64};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestUnreachable {
    V4 {
        header: icmpv4::DestUnreachableHeader,
        total_length: u16,
    },
    V6Unreachable(icmpv6::DestUnreachableCode),
    V6PacketTooBig {
        mtu: u32,
    },
}

impl DestUnreachable {
    pub fn into_icmp_v4_type(self) -> Result<Icmpv4Type> {
        let header = match self {
            DestUnreachable::V4 { header, .. } => header,
            DestUnreachable::V6Unreachable(code) => nat64::translate_dest_unreachable(code)
                .with_context(|| format!("Cannot translate {code:?} to ICMPv4"))?,
            DestUnreachable::V6PacketTooBig { mtu } => nat64::translate_packet_too_big(mtu),
        };

        Ok(Icmpv4Type::DestinationUnreachable(header))
    }

    pub fn into_icmp_v6_type(self) -> Result<Icmpv6Type> {
        match self {
            DestUnreachable::V4 {
                header,
                total_length,
            } => {
                let icmpv6_type =
                    nat46::translate_icmp_unreachable(header.clone(), total_length)
                        .with_context(|| format!("Cannot translate {header:?} to ICMPv6"))?;

                Ok(icmpv6_type)
            }
            DestUnreachable::V6Unreachable(code) => Ok(Icmpv6Type::DestinationUnreachable(code)),
            DestUnreachable::V6PacketTooBig { mtu } => Ok(Icmpv6Type::PacketTooBig { mtu }),
        }
    }
}

/// A packet that failed to route to its destination, extracted from the payload of an ICMP/ICMP6 error message.
#[derive(Debug, PartialEq, Eq)]
pub struct FailedPacket {
    pub(crate) src: IpAddr,
    pub(crate) failed_dst: IpAddr,
    pub(crate) l4_proto: Layer4Protocol,

    pub(crate) raw: Vec<u8>,
}

impl FailedPacket {
    /// The destination we failed to route to.
    pub fn dst(&self) -> IpAddr {
        self.failed_dst
    }

    pub fn src(&self) -> IpAddr {
        self.src
    }

    /// The source protocol of the packet.
    pub fn src_proto(&self) -> Protocol {
        match self.l4_proto {
            Layer4Protocol::Udp { src, .. } => Protocol::Udp(src),
            Layer4Protocol::Tcp { src, .. } => Protocol::Tcp(src),
            Layer4Protocol::Icmp { id, .. } => Protocol::Icmp(id),
        }
    }

    pub fn layer4_protocol(&self) -> Layer4Protocol {
        self.l4_proto
    }

    /// Translates the failed packet to point to the new `destination` address and originating from the given `src_proto`.
    ///
    /// The new `dst` address may be a different IP version than the original packet in which case NAT64/NAT46 will be applied.
    pub fn translate_destination(
        mut self,
        dst: IpAddr,
        src_proto: Protocol,
        src_v4: Ipv4Addr,
        src_v6: Ipv6Addr,
    ) -> Result<Vec<u8>> {
        match (self.failed_dst, dst) {
            (IpAddr::V4(_), IpAddr::V4(dst)) => {
                translate_original_ipv4_packet(self.raw, dst, src_proto)
            }
            (IpAddr::V6(_), IpAddr::V6(dst)) => {
                translate_original_ipv6_packet(self.raw, dst, src_proto)
            }
            (IpAddr::V6(_), IpAddr::V4(dst)) => {
                nat64::translate_in_place(&mut self.raw, src_v4, dst)
                    .context("Failed to translate unrouteable IPv6 packet to IPv4")?;
                let packet = self.raw.split_off(20);

                translate_original_ipv4_packet(packet, dst, src_proto)
            }
            (IpAddr::V4(_), IpAddr::V6(dst)) => {
                let mut packet = [vec![0u8; 20], self.raw].concat();

                let offset = nat46::translate_in_place(&mut packet, src_v6, dst)
                    .context("Failed to translate unrouteable IPv4 packet to IPv6")?;
                let packet = packet.split_off(offset);

                translate_original_ipv6_packet(packet, dst, src_proto)
            }
        }
    }
}

/// Translates the original packet embedded in an ICMP error message to account for the NAT table.
///
/// The ICMP error was generated by a network device on the path between Gateway and Resource.
/// Hence, it contains the actual destination IP of the resource and the source port assigned in the NAT table.
///
/// The client doesn't know about this, meaning we need to translate the destination IP and source port to those on the "inside" of the NAT table.
fn translate_original_ipv4_packet(
    mut original_packet: Vec<u8>,
    inside_dst: Ipv4Addr,
    inside_proto: Protocol,
) -> Result<Vec<u8>> {
    // `original_packet` is an IPv4 packet, thus the destination IP is found from byte 16..20 in the header.
    original_packet[16..20].copy_from_slice(&inside_dst.octets());

    let (lax_ipv4_slice, _) = LaxIpv4Slice::from_slice(&original_packet)
        .context("Failed to parse original packet as `LaxIpv4Slice`")?;

    debug_assert_eq!(
        lax_ipv4_slice.header().destination_addr(),
        inside_dst,
        "Should have modified the destination address correctly"
    );

    let header_len = lax_ipv4_slice.header().slice().len();
    let extension_len = lax_ipv4_slice
        .extensions()
        .auth
        .map(|auth| auth.slice().len())
        .unwrap_or(0);
    let payload_start = header_len + extension_len;

    translate_original_packet_protocol(&mut original_packet, payload_start, inside_proto);

    Ok(original_packet)
}

/// Translates the original packet embedded in an ICMPv6 error message to account for the NAT table.
///
/// The ICMP error was generated by a network device on the path between Gateway and Resource.
/// Hence, it contains the actual destination IP of the resource and the source port assigned in the NAT table.
///
/// The client doesn't know about this, meaning we need to translate the destination IP and source port to those on the "inside" of the NAT table.
fn translate_original_ipv6_packet(
    mut original_packet: Vec<u8>,
    inside_dst: Ipv6Addr,
    inside_proto: Protocol,
) -> Result<Vec<u8>> {
    // `original_packet` is an IPv6 packet, thus the destination IP is found from byte 24..40 in the header.
    original_packet[24..40].copy_from_slice(&inside_dst.octets());

    let (lax_ipv6_slice, _) = LaxIpv6Slice::from_slice(&original_packet)
        .context("Failed to parse original packet as `LaxIpv6Slice`")?;

    debug_assert_eq!(
        lax_ipv6_slice.header().destination_addr(),
        inside_dst,
        "Should have modified the destination address correctly"
    );

    let header_len = lax_ipv6_slice.header().slice().len();
    let extension_len = lax_ipv6_slice.extensions().slice().len();
    let payload_start = header_len + extension_len;

    translate_original_packet_protocol(&mut original_packet, payload_start, inside_proto);

    Ok(original_packet)
}

fn translate_original_packet_protocol(
    original_packet: &mut [u8],
    payload_start: usize,
    inside_proto: Protocol,
) {
    let proto_offset = match inside_proto {
        Protocol::Tcp(_) => 0,  // source port is the first thing in a TCP packet.
        Protocol::Udp(_) => 0,  // source port is the first thing in a UDP packet.
        Protocol::Icmp(_) => 4, // icmp identifier comes after type, code and checksum.
    };
    let proto_index = payload_start + proto_offset;

    original_packet[proto_index..(proto_index + 2)]
        .copy_from_slice(&inside_proto.value().to_be_bytes());
}
