use anyhow::{ensure, Error};
use byteorder::{BigEndian, ByteOrder};

const DNS_HEADER_SIZE: usize = 12;
const DNS_MAX_HOSTNAME_SIZE: usize = 256;
const DNS_MAX_PACKET_SIZE: usize = 65_535;
const DNS_OFFSET_QUESTION: usize = DNS_HEADER_SIZE;
const DNS_TYPE_OPT: u16 = 41;

const DNS_RCODE_SERVFAIL: u8 = 2;
const DNS_RCODE_REFUSED: u8 = 5;

#[inline]
pub fn rcode(packet: &[u8]) -> u8 {
    packet[3] & 0x0f
}

#[inline]
pub fn qdcount(packet: &[u8]) -> u16 {
    BigEndian::read_u16(&packet[4..])
}

#[inline]
pub fn ancount(packet: &[u8]) -> u16 {
    BigEndian::read_u16(&packet[6..])
}

#[inline]
pub fn arcount(packet: &[u8]) -> u16 {
    BigEndian::read_u16(&packet[10..])
}

fn arcount_inc(packet: &mut [u8]) -> Result<(), Error> {
    let mut arcount = arcount(packet);
    ensure!(arcount < 0xffff, "Too many additional records");
    arcount += 1;
    BigEndian::write_u16(&mut packet[10..], arcount);
    Ok(())
}

#[inline]
fn nscount(packet: &[u8]) -> u16 {
    BigEndian::read_u16(&packet[8..])
}

#[inline]
pub fn is_recoverable_error(packet: &[u8]) -> bool {
    let rcode = rcode(packet);
    rcode == DNS_RCODE_SERVFAIL || rcode == DNS_RCODE_REFUSED
}

fn skip_name(packet: &[u8], offset: usize) -> Result<usize, Error> {
    let packet_len = packet.len();
    ensure!(offset < packet_len - 1, "Short packet");
    let mut qname_len: usize = 0;
    let mut offset = offset;
    loop {
        let label_len = match packet[offset] as usize {
            label_len if label_len & 0xc0 == 0xc0 => {
                ensure!(packet_len - offset >= 2, "Incomplete offset");
                offset += 2;
                break;
            }
            label_len => label_len,
        } as usize;
        ensure!(
            packet_len - offset - 1 > label_len,
            "Malformed packet with an out-of-bounds name"
        );
        qname_len += label_len + 1;
        ensure!(qname_len <= DNS_MAX_HOSTNAME_SIZE, "Name too long");
        offset += label_len + 1;
        if label_len == 0 {
            break;
        }
    }
    Ok(offset)
}

fn traverse_rrs<F: FnMut(usize) -> Result<(), Error>>(
    packet: &[u8],
    mut offset: usize,
    rrcount: u16,
    mut cb: F,
) -> Result<usize, Error> {
    let packet_len = packet.len();
    for _ in 0..rrcount {
        offset = skip_name(packet, offset)?;
        ensure!(packet_len - offset >= 10, "Short packet");
        cb(offset)?;
        let rdlen = BigEndian::read_u16(&packet[offset + 8..]) as usize;
        offset += 10;
        ensure!(
            packet_len - offset >= rdlen,
            "Record length would exceed packet length"
        );
        offset += rdlen;
    }
    Ok(offset)
}

fn traverse_rrs_mut<F: FnMut(&mut [u8], usize) -> Result<(), Error>>(
    packet: &mut [u8],
    mut offset: usize,
    rrcount: u16,
    mut cb: F,
) -> Result<usize, Error> {
    let packet_len = packet.len();
    for _ in 0..rrcount {
        offset = skip_name(packet, offset)?;
        ensure!(packet_len - offset >= 10, "Short packet");
        cb(packet, offset)?;
        let rdlen = BigEndian::read_u16(&packet[offset + 8..]) as usize;
        offset += 10;
        ensure!(
            packet_len - offset >= rdlen,
            "Record length would exceed packet length"
        );
        offset += rdlen;
    }
    Ok(offset)
}

pub fn min_ttl(packet: &[u8], min_ttl: u32, max_ttl: u32, failure_ttl: u32) -> Result<u32, Error> {
    ensure!(qdcount(packet) == 1, "Unsupported number of questions");
    let packet_len = packet.len();
    ensure!(packet_len > DNS_OFFSET_QUESTION, "Short packet");
    ensure!(packet_len <= DNS_MAX_PACKET_SIZE, "Large packet");
    let mut offset = skip_name(packet, DNS_OFFSET_QUESTION)?;
    assert!(offset > DNS_OFFSET_QUESTION);
    ensure!(packet_len - offset > 4, "Short packet");
    offset += 4;
    let (ancount, nscount, arcount) = (ancount(packet), nscount(packet), arcount(packet));
    let rrcount = ancount + nscount + arcount;
    let mut found_min_ttl = if rrcount > 0 { max_ttl } else { failure_ttl };

    offset = traverse_rrs(packet, offset, rrcount, |offset| {
        let qtype = BigEndian::read_u16(&packet[offset..]);
        let ttl = BigEndian::read_u32(&packet[offset + 4..]);
        if qtype != DNS_TYPE_OPT && ttl < found_min_ttl {
            found_min_ttl = ttl;
        }
        Ok(())
    })?;
    if found_min_ttl < min_ttl {
        found_min_ttl = min_ttl;
    }
    ensure!(packet_len == offset, "Garbage after packet");
    Ok(found_min_ttl)
}

fn add_edns_section(packet: &mut Vec<u8>, max_payload_size: u16) -> Result<(), Error> {
    let opt_rr: [u8; 11] = [
        0,
        (DNS_TYPE_OPT >> 8) as u8,
        DNS_TYPE_OPT as u8,
        (max_payload_size >> 8) as u8,
        max_payload_size as u8,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    ensure!(
        DNS_MAX_PACKET_SIZE - packet.len() >= opt_rr.len(),
        "Packet would be too large to add a new record"
    );
    arcount_inc(packet)?;
    packet.extend(&opt_rr);
    Ok(())
}

pub fn set_edns_max_payload_size(packet: &mut Vec<u8>, max_payload_size: u16) -> Result<(), Error> {
    ensure!(qdcount(packet) == 1, "Unsupported number of questions");
    let packet_len = packet.len();
    ensure!(packet_len > DNS_OFFSET_QUESTION, "Short packet");
    ensure!(packet_len <= DNS_MAX_PACKET_SIZE, "Large packet");

    let mut offset = skip_name(packet, DNS_OFFSET_QUESTION)?;
    assert!(offset > DNS_OFFSET_QUESTION);
    ensure!(packet_len - offset >= 4, "Short packet");
    offset += 4;
    let (ancount, nscount, arcount) = (ancount(packet), nscount(packet), arcount(packet));
    offset = traverse_rrs(packet, offset, ancount + nscount, |_offset| Ok(()))?;
    let mut edns_payload_set = false;
    traverse_rrs_mut(packet, offset, arcount, |packet, offset| {
        let qtype = BigEndian::read_u16(&packet[offset..]);
        if qtype == DNS_TYPE_OPT {
            ensure!(!edns_payload_set, "Duplicate OPT RR found");
            BigEndian::write_u16(&mut packet[offset + 2..], max_payload_size);
            edns_payload_set = true;
        }
        Ok(())
    })?;
    if edns_payload_set {
        return Ok(());
    }
    add_edns_section(packet, max_payload_size)?;
    Ok(())
}
