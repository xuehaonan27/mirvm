#!/usr/bin/env mirvm
---
[dependencies]
smoltcp = { version = "0.12", default-features = false, features = [
    "std",
    "medium-ip",
    "proto-ipv4",
    "socket-tcp",
    "socket-udp",
    "socket-icmp",
] }
---
// smoltcp differential over two lines, compared byte-for-byte with native.
// (1) A loopback device with a hand-driven Instant clock: two TCP sockets on one
//     interface echo to each other, Config.random_seed pinned (the ISN comes from
//     the iface's xorshift Rand), explicit local ports and a poll loop stepping
//     1ms from Instant::ZERO. It prints the state transition pairs, each
//     send/receive length and FNV, the roundtrip boolean and the FIN close
//     sequence; no NIC and no printed timestamps or sequence numbers.
// (2) The wire codec: IPv4/TCP/UDP/ICMPv4 packets are built byte by byte with a
//     hand-rolled RFC 1071 checksum and parsed twice, through new_checked then
//     Repr::parse, printing every field and the checksum boolean (TCP also parses
//     MSS). Malformed packets cover truncation, bad lengths, a bad checksum and a
//     wrong version (the packet layer accepts, Repr rejects).
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{ChecksumCapabilities, Loopback, Medium};
use smoltcp::socket::tcp::{Socket as TcpSocket, SocketBuffer, State as TcpState};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{
    HardwareAddress, Icmpv4Packet, IpAddress, IpCidr, Ipv4Address, Ipv4Packet, Ipv4Repr, TcpPacket,
    TcpRepr, UdpPacket,
};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// RFC 1071 internet checksum: one's complement sum of 16-bit big-endian words.
/// A trailing byte of an odd-length part pairs with the next part's first byte; padding at the end.
fn inet_checksum(parts: &[&[u8]]) -> u16 {
    let mut sum: u32 = 0;
    let mut pending: Option<u8> = None;
    for part in parts {
        let mut slice = *part;
        if let Some(hi) = pending.take() {
            match slice.split_first() {
                Some((&lo, rest)) => {
                    sum += u16::from_be_bytes([hi, lo]) as u32;
                    slice = rest;
                }
                None => {
                    pending = Some(hi);
                    continue;
                }
            }
        }
        let mut chunks = slice.chunks_exact(2);
        for c in &mut chunks {
            sum += u16::from_be_bytes([c[0], c[1]]) as u32;
        }
        if let Some(&last) = chunks.remainder().first() {
            pending = Some(last);
        }
    }
    if let Some(hi) = pending {
        sum += u16::from_be_bytes([hi, 0]) as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

const SRC: [u8; 4] = [192, 0, 2, 1];
const DST: [u8; 4] = [192, 0, 2, 2];

/// Fixed 20-byte IPv4 header + payload, DF set, TTL=64, checksum computed here.
fn build_ipv4(proto: u8, payload: &[u8], ident: u16) -> Vec<u8> {
    let total = (20 + payload.len()) as u16;
    let mut b = Vec::with_capacity(total as usize);
    b.push(0x45); // version=4, IHL=5
    b.push(0); // DSCP/ECN
    b.extend_from_slice(&total.to_be_bytes());
    b.extend_from_slice(&ident.to_be_bytes());
    b.extend_from_slice(&0x4000u16.to_be_bytes()); // DF, frag_off=0
    b.push(64); // TTL
    b.push(proto);
    b.extend_from_slice(&[0, 0]); // checksum placeholder
    b.extend_from_slice(&SRC);
    b.extend_from_slice(&DST);
    let csum = inet_checksum(&[&b]);
    b[10..12].copy_from_slice(&csum.to_be_bytes());
    b.extend_from_slice(payload);
    b
}

/// TCP segment: 20-byte header + options (4-byte aligned) + payload, pseudo-header checksum.
#[allow(clippy::too_many_arguments)]
fn build_tcp(
    sport: u16,
    dport: u16,
    seq: i32,
    ack: i32,
    flags: u8,
    options: &[u8],
    window: u16,
    payload: &[u8],
) -> Vec<u8> {
    assert_eq!(options.len() % 4, 0);
    let doff = ((20 + options.len()) / 4) as u8;
    let mut s = Vec::new();
    s.extend_from_slice(&sport.to_be_bytes());
    s.extend_from_slice(&dport.to_be_bytes());
    s.extend_from_slice(&seq.to_be_bytes());
    s.extend_from_slice(&ack.to_be_bytes());
    s.push(doff << 4);
    s.push(flags);
    s.extend_from_slice(&window.to_be_bytes());
    s.extend_from_slice(&[0, 0]); // checksum placeholder
    s.extend_from_slice(&[0, 0]); // urgent
    s.extend_from_slice(options);
    s.extend_from_slice(payload);
    let pseudo = pseudo_header(6, s.len() as u32);
    let csum = inet_checksum(&[&pseudo, &s]);
    s[16..18].copy_from_slice(&csum.to_be_bytes());
    s
}

/// UDP datagram: 8-byte header + payload, pseudo-header checksum (csum_override forces 0).
fn build_udp(sport: u16, dport: u16, payload: &[u8], csum_override: Option<u16>) -> Vec<u8> {
    let len = (8 + payload.len()) as u16;
    let mut u = Vec::new();
    u.extend_from_slice(&sport.to_be_bytes());
    u.extend_from_slice(&dport.to_be_bytes());
    u.extend_from_slice(&len.to_be_bytes());
    u.extend_from_slice(&[0, 0]);
    u.extend_from_slice(payload);
    let csum = match csum_override {
        Some(c) => c,
        None => {
            let pseudo = pseudo_header(17, len as u32);
            inet_checksum(&[&pseudo, &u])
        }
    };
    u[6..8].copy_from_slice(&csum.to_be_bytes());
    u
}

/// ICMPv4 echo request: type=8 code=0 + ident/seq + data, checksum computed here.
fn build_icmp_echo(ident: u16, seq: u16, data: &[u8]) -> Vec<u8> {
    let mut m = Vec::new();
    m.push(8); // echo request
    m.push(0); // code
    m.extend_from_slice(&[0, 0]); // checksum placeholder
    m.extend_from_slice(&ident.to_be_bytes());
    m.extend_from_slice(&seq.to_be_bytes());
    m.extend_from_slice(data);
    let csum = inet_checksum(&[&m]);
    m[2..4].copy_from_slice(&csum.to_be_bytes());
    m
}

fn pseudo_header(proto: u8, len: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(12);
    p.extend_from_slice(&SRC);
    p.extend_from_slice(&DST);
    p.push(0);
    p.push(proto);
    p.extend_from_slice(&(len as u16).to_be_bytes());
    p
}

/// Recomputes the IPv4 header checksum so a mutated header stays valid (isolating the variable).
fn fix_ipv4_csum(b: &mut [u8]) {
    b[10] = 0;
    b[11] = 0;
    let ihl = ((b[0] & 0x0f) as usize) * 4;
    let c = inet_checksum(&[&b[..ihl]]);
    b[10..12].copy_from_slice(&c.to_be_bytes());
}

fn src_ip() -> IpAddress {
    IpAddress::Ipv4(Ipv4Address::new(SRC[0], SRC[1], SRC[2], SRC[3]))
}

fn dst_ip() -> IpAddress {
    IpAddress::Ipv4(Ipv4Address::new(DST[0], DST[1], DST[2], DST[3]))
}

/// (1) TCP client/server echo over loopback, driven by the hand-built clock.
fn tcp_echo() {
    let mut device = Loopback::new(Medium::Ip);
    let mut config = Config::new(HardwareAddress::Ip);
    config.random_seed = 0x5EED_5EED_5EED_5EED;
    let mut iface = Interface::new(config, &mut device, Instant::ZERO);
    iface.update_ip_addrs(|addrs| {
        addrs.push(IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24)).unwrap();
    });

    let mut server = TcpSocket::new(
        SocketBuffer::new(vec![0; 2048]),
        SocketBuffer::new(vec![0; 2048]),
    );
    server.listen(1234).unwrap();
    let mut client = TcpSocket::new(
        SocketBuffer::new(vec![0; 2048]),
        SocketBuffer::new(vec![0; 2048]),
    );
    client
        .connect(iface.context(), (IpAddress::v4(10, 0, 0, 1), 1234u16), 49152u16)
        .unwrap();

    let mut sockets = SocketSet::new(vec![]);
    let server_h = sockets.add(server);
    let client_h = sockets.add(client);

    let big = vec![0xABu8; 700];
    let msgs: [&[u8]; 3] = [
        b"ping from client",
        b"second message: smoltcp loopback echo",
        &big,
    ];
    let mut expect: Vec<u8> = Vec::new();
    for m in msgs {
        expect.extend_from_slice(m);
    }
    let total = expect.len();

    let mut now = Instant::ZERO;
    let mut step = 0u32;
    let mut sent = 0usize;
    let mut recv_acc: Vec<u8> = Vec::new();
    let mut closed = false;
    let mut server_closed = false;
    let mut prev: Option<(TcpState, TcpState)> = None;
    let mut sbuf = [0u8; 2048];
    let mut cbuf = [0u8; 2048];

    let final_states = loop {
        step += 1;
        now += Duration::from_millis(1);
        iface.poll(now, &mut device, &mut sockets);

        let cs = sockets.get::<TcpSocket>(client_h).state();
        let ss = sockets.get::<TcpSocket>(server_h).state();
        if prev != Some((cs, ss)) {
            println!("t={}ms client={} server={}", now.total_millis(), cs, ss);
            prev = Some((cs, ss));
        }

        if sent < msgs.len() {
            let c = sockets.get_mut::<TcpSocket>(client_h);
            if c.state() == TcpState::Established && c.can_send() {
                let m = msgs[sent];
                let n = c.send_slice(m).unwrap();
                assert_eq!(n, m.len());
                println!("send[{sent}] len={n} fnv={:016x}", fnv1a(m));
                sent += 1;
            }
        }

        {
            let s = sockets.get_mut::<TcpSocket>(server_h);
            if s.can_recv() {
                let n = s.recv_slice(&mut sbuf).unwrap();
                if n > 0 {
                    println!("server-recv len={n} fnv={:016x}", fnv1a(&sbuf[..n]));
                    let sent_back = s.send_slice(&sbuf[..n]).unwrap();
                    assert_eq!(sent_back, n);
                }
            }
        }

        {
            let c = sockets.get_mut::<TcpSocket>(client_h);
            if c.can_recv() {
                let n = c.recv_slice(&mut cbuf).unwrap();
                if n > 0 {
                    println!("client-recv len={n} fnv={:016x}", fnv1a(&cbuf[..n]));
                    recv_acc.extend_from_slice(&cbuf[..n]);
                }
            }
        }

        if !closed && sent == msgs.len() && recv_acc.len() == total {
            println!("echo roundtrip = {}", recv_acc == expect);
            sockets.get_mut::<TcpSocket>(client_h).close();
            closed = true;
        }
        if closed && !server_closed {
            let s = sockets.get_mut::<TcpSocket>(server_h);
            if s.state() == TcpState::CloseWait {
                s.close();
                server_closed = true;
            }
        }

        let cs = sockets.get::<TcpSocket>(client_h).state();
        let ss = sockets.get::<TcpSocket>(server_h).state();
        if closed && ss == TcpState::Closed {
            break (cs, ss);
        }
        if step > 20000 {
            println!("loop cap hit");
            break (cs, ss);
        }
    };
    println!(
        "final client={} server={} steps={step}",
        final_states.0, final_states.1
    );
}

/// (2) wire codec: build -> parse -> print fields and checksum booleans; malformed paths.
fn codec() {
    let caps = ChecksumCapabilities::default();

    // A) IPv4/TCP SYN carrying the MSS option
    let tcp = build_tcp(
        49152,
        80,
        0x0102_0304,
        0,
        0x02, // SYN
        &[0x02, 0x04, 0x05, 0xb4, 0x01, 0x00, 0x00, 0x00], // MSS=1460 NOP EOL pad
        64240,
        b"",
    );
    let ip_a = build_ipv4(6, &tcp, 0xBEEF);
    let p = Ipv4Packet::new_checked(&ip_a).unwrap();
    println!(
        "A ipv4 ver={} ihl={} total={} ident={:#06x} df={} mf={} foff={} hop={} proto={:?} csum_ok={}",
        p.version(),
        p.header_len(),
        p.total_len(),
        p.ident(),
        p.dont_frag(),
        p.more_frags(),
        p.frag_offset(),
        p.hop_limit(),
        p.next_header(),
        p.verify_checksum()
    );
    println!("A ipv4 src={} dst={}", p.src_addr(), p.dst_addr());
    let r = Ipv4Repr::parse(&p, &caps).unwrap();
    println!(
        "A ipv4-repr next={:?} hop_limit={} payload_len={}",
        r.next_header, r.hop_limit, r.payload_len
    );
    let t = TcpPacket::new_checked(p.payload()).unwrap();
    println!(
        "A tcp {}->{} seq={:#010x} ack={:#010x} doff={} syn={} ack={} fin={} rst={} psh={} win={} urg={} csum_ok={} payload={}",
        t.src_port(),
        t.dst_port(),
        t.seq_number().0,
        t.ack_number().0,
        t.header_len(),
        t.syn(),
        t.ack(),
        t.fin(),
        t.rst(),
        t.psh(),
        t.window_len(),
        t.urgent_at(),
        t.verify_checksum(&src_ip(), &dst_ip()),
        t.payload().len()
    );
    let tr = TcpRepr::parse(&t, &src_ip(), &dst_ip(), &caps).unwrap();
    println!(
        "A tcp-repr ctrl={:?} mss={:?} sack_perm={} ws={:?} win={}",
        tr.control, tr.max_seg_size, tr.sack_permitted, tr.window_scale, tr.window_len
    );

    // B) IPv4/UDP with a text payload
    let udp = build_udp(8080, 53, b"hello udp wire", None);
    let ip_b = build_ipv4(17, &udp, 0x1234);
    let pb = Ipv4Packet::new_checked(&ip_b).unwrap();
    let u = UdpPacket::new_checked(pb.payload()).unwrap();
    println!(
        "B udp {}->{} len={} csum={:#06x} csum_ok={} payload_len={} payload={}",
        u.src_port(),
        u.dst_port(),
        u.len(),
        u.checksum(),
        u.verify_checksum(&src_ip(), &dst_ip()),
        u.payload().len(),
        std::str::from_utf8(u.payload()).unwrap()
    );

    // C) IPv4/ICMPv4 echo request
    let icmp = build_icmp_echo(0x1234, 0x0001, b"icmp-echo-data");
    let ip_c = build_ipv4(1, &icmp, 0x7777);
    let pc = Ipv4Packet::new_checked(&ip_c).unwrap();
    let ic = Icmpv4Packet::new_checked(pc.payload()).unwrap();
    println!(
        "C icmp type={:?} code={} csum_ok={} hdr_len={} data_len={} data={}",
        ic.msg_type(),
        ic.msg_code(),
        ic.verify_checksum(),
        ic.header_len(),
        ic.data().len(),
        std::str::from_utf8(ic.data()).unwrap()
    );

    // D) malformed packet error paths (wire::Error Display is always "wire::Error")
    match Ipv4Packet::new_checked(&ip_a[..10]) {
        Ok(_) => println!("D1 unexpected ok"),
        Err(e) => println!("D1 ipv4-truncated err = {e}"),
    }
    let mut m2 = ip_a.clone();
    m2[0] = 0x4F; // IHL=15 → header 60 > total
    match Ipv4Packet::new_checked(&m2) {
        Ok(_) => println!("D2 unexpected ok"),
        Err(e) => println!("D2 ipv4-ihl-gt-total err = {e}"),
    }
    let mut m3 = ip_a.clone();
    m3[2..4].copy_from_slice(&60u16.to_be_bytes()); // total_len=60 but the buffer is only 48
    match Ipv4Packet::new_checked(&m3) {
        Ok(_) => println!("D3 unexpected ok"),
        Err(e) => println!("D3 ipv4-total-gt-buf err = {e}"),
    }
    let mut m4 = ip_a.clone();
    m4[20] ^= 0xFF; // corrupt the first TCP byte; the IP header checksum stays valid
    let p4 = Ipv4Packet::new_checked(&m4).unwrap();
    println!("D4 ipv4 csum_ok={} (腐化在 payload)", p4.verify_checksum());
    let t4 = TcpPacket::new_checked(p4.payload()).unwrap();
    println!(
        "D4 tcp csum_ok={}",
        t4.verify_checksum(&src_ip(), &dst_ip())
    );
    match TcpRepr::parse(&t4, &src_ip(), &dst_ip(), &caps) {
        Ok(_) => println!("D4 tcp-repr unexpected ok"),
        Err(e) => println!("D4 tcp-repr err = {e}"),
    }
    let mut m5 = ip_a.clone();
    m5[10] ^= 0x01; // corrupt the IP header checksum field
    let p5 = Ipv4Packet::new_checked(&m5).unwrap();
    println!("D5 ipv4 csum_ok={}", p5.verify_checksum());
    match Ipv4Repr::parse(&p5, &caps) {
        Ok(_) => println!("D5 ipv4-repr unexpected ok"),
        Err(e) => println!("D5 ipv4-repr err = {e}"),
    }
    let mut m6 = ip_a.clone();
    m6[0] = 0x65; // version=6 (IHL still 5), checksum recomputed to isolate the variable
    fix_ipv4_csum(&mut m6);
    let p6 = Ipv4Packet::new_checked(&m6).unwrap();
    println!(
        "D6 ver={} csum_ok={} (packet 层不查版本)",
        p6.version(),
        p6.verify_checksum()
    );
    match Ipv4Repr::parse(&p6, &caps) {
        Ok(_) => println!("D6 ipv4-repr unexpected ok"),
        Err(e) => println!("D6 ipv4-repr err = {e}"),
    }
    match TcpPacket::new_checked(&tcp[..10]) {
        Ok(_) => println!("D7 unexpected ok"),
        Err(e) => println!("D7 tcp-truncated err = {e}"),
    }
    let mut m8 = tcp.clone();
    m8[12] = 4 << 4; // data_offset=4 < 5
    match TcpPacket::new_checked(&m8) {
        Ok(_) => println!("D8 unexpected ok"),
        Err(e) => println!("D8 tcp-doff-too-small err = {e}"),
    }
    let mut m9 = udp.clone();
    m9[4..6].copy_from_slice(&4u16.to_be_bytes()); // UDP length=4 < 8
    match UdpPacket::new_checked(&m9) {
        Ok(_) => println!("D9 unexpected ok"),
        Err(e) => println!("D9 udp-len-too-small err = {e}"),
    }
    let mut m10 = udp.clone();
    m10[4..6].copy_from_slice(&64u16.to_be_bytes()); // length=64 against a 22-byte buffer
    match UdpPacket::new_checked(&m10) {
        Ok(_) => println!("D10 unexpected ok"),
        Err(e) => println!("D10 udp-len-gt-buf err = {e}"),
    }
    let u_zero = build_udp(8080, 53, b"hello udp wire", Some(0));
    let uz = UdpPacket::new_checked(&u_zero).unwrap();
    println!(
        "D11 udp csum=0 → verify={}（IPv4 下免校验语义）",
        uz.verify_checksum(&src_ip(), &dst_ip())
    );
    match Icmpv4Packet::new_checked(&icmp[..4]) {
        Ok(_) => println!("D12 unexpected ok"),
        Err(e) => println!("D12 icmp-truncated err = {e}"),
    }
    let mut m13 = icmp.clone();
    m13[4] ^= 0xFF; // corrupt ident so the checksum no longer matches
    let ic13 = Icmpv4Packet::new_checked(&m13).unwrap();
    println!("D13 icmp csum_ok={}", ic13.verify_checksum());
}

fn main() {
    tcp_echo();
    codec();
}
