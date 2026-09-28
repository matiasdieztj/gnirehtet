#![allow(dead_code)] // used by feat/quic-reject; removed in the next commit
//! Synthesizes ICMP Echo Replies for Echo Requests coming from the device.
//!
//! We deliberately do NOT forward ICMP to the real destination: that would
//! require raw sockets (root on the host), which gnirehtet avoids. Instead,
//! every Echo Request gets an immediate synthetic Echo Reply, so apps that
//! rely on ping for "is the tunnel alive?" checks (WiFiman, etc.) see success.

use super::ip_header::IpHeaderData;
use super::ip_packet::IpPacket;
use byteorder::{BigEndian, ByteOrder};

const IPV4_HEADER_MIN_LEN: usize = 20;
const ICMP_HEADER_LEN: usize = 8;
const PROTOCOL_ICMP: u8 = 1;
const ICMP_TYPE_ECHO_REQUEST: u8 = 8;
const ICMP_TYPE_ECHO_REPLY: u8 = 0;

/// If `request` is a valid IPv4 ICMP Echo Request, return a byte buffer
/// holding the corresponding Echo Reply (addresses swapped, both checksums
/// recomputed). Returns `None` for anything else.
pub fn build_echo_reply(request: &[u8]) -> Option<Vec<u8>> {
    if request.len() < IPV4_HEADER_MIN_LEN + ICMP_HEADER_LEN {
        return None;
    }
    if (request[0] >> 4) != 4 {
        return None;
    }
    let ihl = usize::from(request[0] & 0x0f) * 4;
    if ihl < IPV4_HEADER_MIN_LEN || request.len() < ihl + ICMP_HEADER_LEN {
        return None;
    }
    if request[9] != PROTOCOL_ICMP || request[ihl] != ICMP_TYPE_ECHO_REQUEST {
        return None;
    }
    let total_length = usize::from(BigEndian::read_u16(&request[2..4]));
    if total_length > request.len() || total_length < ihl + ICMP_HEADER_LEN {
        return None;
    }

    let mut reply = request[..total_length].to_vec();

    // Swap IPv4 source (offsets 12..16) and destination (offsets 16..20)
    for i in 0..4 {
        reply.swap(12 + i, 16 + i);
    }

    // ICMP type: Echo Request (8) → Echo Reply (0)
    reply[ihl] = ICMP_TYPE_ECHO_REPLY;

    // Recompute ICMP checksum
    reply[ihl + 2] = 0;
    reply[ihl + 3] = 0;
    let icmp_sum = checksum(&reply[ihl..total_length]);
    BigEndian::write_u16(&mut reply[ihl + 2..ihl + 4], icmp_sum);

    // Recompute IPv4 header checksum
    reply[10] = 0;
    reply[11] = 0;
    let ip_sum = checksum(&reply[..ihl]);
    BigEndian::write_u16(&mut reply[10..12], ip_sum);

    Some(reply)
}

/// Build an IPv4 + ICMP Port Unreachable packet (Type 3, Code 3) responding
/// to `original`, following RFC 792 §3:
///
///   * the ICMP payload contains the original IP header, plus the first
///     8 bytes of the original datagram's payload;
///   * source and destination IPs are swapped;
///   * both IP and ICMP checksums are recomputed.
///
/// Returns `None` for non-IPv4 packets (IPv6 ICMP is not implemented).
pub fn build_port_unreachable(original: &IpPacket) -> Option<Vec<u8>> {
    let (ip_data, _) = original.headers_data();
    let IpHeaderData::V4(v4) = ip_data else {
        return None;
    };

    let src = v4.destination().to_be_bytes();
    let dst = v4.source().to_be_bytes();

    let orig_raw = original.raw();
    let orig_header_len = v4.header_length() as usize;
    if orig_raw.len() < orig_header_len {
        return None;
    }
    let copy_len = (orig_raw.len() - orig_header_len).min(8);
    let icmp_payload_len = orig_header_len + copy_len;
    let icmp_len = ICMP_HEADER_LEN + icmp_payload_len;
    let total_len = IPV4_HEADER_MIN_LEN + icmp_len;

    let mut buf = vec![0u8; total_len];

    // ── IPv4 header ──
    buf[0] = 0x45; // version 4, IHL 5
    buf[8] = 64; // TTL
    buf[9] = PROTOCOL_ICMP;
    BigEndian::write_u16(&mut buf[2..4], total_len as u16);
    buf[12..16].copy_from_slice(&src);
    buf[16..20].copy_from_slice(&dst);
    let ip_sum = checksum(&buf[..IPV4_HEADER_MIN_LEN]);
    BigEndian::write_u16(&mut buf[10..12], ip_sum);

    // ── ICMP header ──
    buf[20] = 3; // type = Destination Unreachable
    buf[21] = 3; // code = Port Unreachable
    // buf[22..24] = checksum, filled below
    // buf[24..28] = unused, already zero

    // ── ICMP payload (original IP header + first 8 bytes of payload) ──
    buf[28..28 + orig_header_len].copy_from_slice(&orig_raw[..orig_header_len]);
    buf[28 + orig_header_len..28 + icmp_payload_len]
        .copy_from_slice(&orig_raw[orig_header_len..orig_header_len + copy_len]);

    let icmp_sum = checksum(&buf[IPV4_HEADER_MIN_LEN..total_len]);
    BigEndian::write_u16(&mut buf[22..24], icmp_sum);

    Some(buf)
}

/// Standard 16-bit one's-complement checksum (RFC 1071).
fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u32::from(BigEndian::read_u16(&data[i..i + 2]));
        i += 2;
    }
    if i < data.len() {
        sum += u32::from(data[i]) << 8;
    }
    while (sum & !0xFFFF) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use byteorder::{BigEndian, WriteBytesExt};

    fn build_echo_request() -> Vec<u8> {
        let mut raw = Vec::new();
        raw.write_u8(4 << 4 | 5).unwrap(); // version + IHL
        raw.write_u8(0).unwrap(); // ToS
        raw.write_u16::<BigEndian>(20 + 8 + 4).unwrap(); // total length
        raw.write_u32::<BigEndian>(0).unwrap(); // id
        raw.write_u8(64).unwrap(); // TTL
        raw.write_u8(1).unwrap(); // protocol ICMP
        raw.write_u16::<BigEndian>(0).unwrap(); // header checksum
        raw.write_u32::<BigEndian>(0x0A000002).unwrap(); // 10.0.0.2
        raw.write_u32::<BigEndian>(0x08080808).unwrap(); // 8.8.8.8

        raw.write_u8(8).unwrap(); // ICMP type: Echo Request
        raw.write_u8(0).unwrap(); // code
        raw.write_u16::<BigEndian>(0).unwrap(); // ICMP checksum
        raw.write_u16::<BigEndian>(0x1234).unwrap(); // identifier
        raw.write_u16::<BigEndian>(0x0001).unwrap(); // sequence

        raw.write_u32::<BigEndian>(0xDEADBEEF).unwrap(); // payload

        // Fix IP header checksum
        let ip_sum = checksum(&raw[..20]);
        BigEndian::write_u16(&mut raw[10..12], ip_sum);
        // Fix ICMP checksum
        let icmp_sum = checksum(&raw[20..]);
        BigEndian::write_u16(&mut raw[22..24], icmp_sum);

        raw
    }

    #[test]
    fn builds_valid_reply() {
        let req = build_echo_request();
        let reply = build_echo_reply(&req).expect("should build reply");
        assert_eq!(reply.len(), req.len());
        assert_eq!(reply[20], ICMP_TYPE_ECHO_REPLY);
        // source/dest swapped
        assert_eq!(&reply[12..16], &[8, 8, 8, 8]);
        assert_eq!(&reply[16..20], &[10, 0, 0, 2]);
        // payload preserved
        assert_eq!(&reply[28..32], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn rejects_non_icmp() {
        let mut req = build_echo_request();
        req[9] = 6; // change to TCP
        assert!(build_echo_reply(&req).is_none());
    }

    #[test]
    fn rejects_short() {
        assert!(build_echo_reply(&[0u8; 10]).is_none());
    }

    #[test]
    fn port_unreachable_layout() {
        use crate::relay::ip_packet::IpPacket;
        // Build a minimal UDP packet 10.0.0.2:50000 -> 8.8.8.8:443
        let mut raw = Vec::new();
        raw.write_u8(4 << 4 | 5).unwrap(); // IPv4, IHL 5
        raw.write_u8(0).unwrap();
        raw.write_u16::<BigEndian>(28).unwrap(); // 20 IP + 8 UDP
        raw.write_u32::<BigEndian>(0).unwrap();
        raw.write_u8(64).unwrap();
        raw.write_u8(17).unwrap(); // UDP
        raw.write_u16::<BigEndian>(0).unwrap();
        raw.write_u32::<BigEndian>(0x0A000002).unwrap();
        raw.write_u32::<BigEndian>(0x08080808).unwrap();
        raw.write_u16::<BigEndian>(50000).unwrap(); // source port
        raw.write_u16::<BigEndian>(443).unwrap(); // dest port (QUIC)
        raw.write_u16::<BigEndian>(8).unwrap(); // UDP length
        raw.write_u16::<BigEndian>(0).unwrap(); // UDP checksum

        let mut buf = raw;
        let packet = IpPacket::parse(&mut buf).unwrap();
        let reply = build_port_unreachable(&packet).expect("should build");
        assert_eq!(reply[0], 0x45);
        assert_eq!(reply[9], PROTOCOL_ICMP);
        assert_eq!(reply[20], 3); // Destination Unreachable
        assert_eq!(reply[21], 3); // Port Unreachable
        // IPs swapped: src should be 8.8.8.8, dst 10.0.0.2
        assert_eq!(&reply[12..16], &[8, 8, 8, 8]);
        assert_eq!(&reply[16..20], &[10, 0, 0, 2]);
    }
}
