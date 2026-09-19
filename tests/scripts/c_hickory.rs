#!/usr/bin/env mirvm
---
[dependencies]
hickory-proto = "0.26"
---
// hickory-proto 0.26: DNS message encode/decode differential. Query/response is
// built, to_vec-encoded and from_vec-decoded, printing hex, every field, an FNV-1a
// checksum and the roundtrip boolean. Covers 9 RData kinds, compression pointers
// (0xC0), EDNS(OPT), TTL edges, truncate, idna, root name, 4 decode errors; no RNG/time.
use hickory_proto::op::{Edns, Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::rdata::{A, AAAA, CNAME, MX, NS, PTR, SOA, SRV, TXT};
use hickory_proto::rr::{DNSClass, Name, RData, Record, RecordType};
use std::net::{Ipv4Addr, Ipv6Addr};

fn hex(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(HEX[(x >> 4) as usize] as char);
        s.push(HEX[(x & 0xf) as usize] as char);
    }
    s
}

fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn dump_rdata(rd: &RData) -> String {
    match rd {
        RData::A(a) => format!("A {}", a.0),
        RData::AAAA(a) => format!("AAAA {}", a.0),
        RData::CNAME(n) => format!("CNAME {}", n.0.to_ascii()),
        RData::NS(n) => format!("NS {}", n.0.to_ascii()),
        RData::PTR(n) => format!("PTR {}", n.0.to_ascii()),
        RData::MX(mx) => format!("MX pref={} exch={}", mx.preference, mx.exchange.to_ascii()),
        RData::TXT(t) => {
            let parts: Vec<String> = t.txt_data.iter().map(|s| hex(s)).collect();
            format!("TXT [{}]", parts.join(","))
        }
        RData::SOA(s) => format!(
            "SOA mname={} rname={} serial={} refresh={} retry={} expire={} min={}",
            s.mname.to_ascii(),
            s.rname.to_ascii(),
            s.serial,
            s.refresh,
            s.retry,
            s.expire,
            s.minimum
        ),
        RData::SRV(s) => format!(
            "SRV pri={} w={} port={} target={}",
            s.priority,
            s.weight,
            s.port,
            s.target.to_ascii()
        ),
        _ => "other".to_string(),
    }
}

fn dump_message(tag: &str, m: &Message) {
    let md = &m.metadata;
    println!(
        "{tag} hdr id={} type={:?} op={:?} rcode={:?} aa={} tc={} rd={} ra={} ad={} cd={}",
        md.id,
        md.message_type,
        md.op_code,
        md.response_code,
        md.authoritative,
        md.truncation,
        md.recursion_desired,
        md.recursion_available,
        md.authentic_data,
        md.checking_disabled
    );
    println!(
        "{tag} counts q={} an={} ns={} ar={}",
        m.queries.len(),
        m.answers.len(),
        m.authorities.len(),
        m.additionals.len()
    );
    for (i, q) in m.queries.iter().enumerate() {
        println!(
            "{tag} q[{i}] name={} type={:?} class={:?}",
            q.name().to_ascii(),
            q.query_type(),
            q.query_class()
        );
    }
    for (sec, rs) in [("an", &m.answers), ("ns", &m.authorities), ("ar", &m.additionals)] {
        for (i, r) in rs.iter().enumerate() {
            println!(
                "{tag} {sec}[{i}] name={} class={:?} type={:?} ttl={} {}",
                r.name.to_ascii(),
                r.dns_class,
                r.record_type(),
                r.ttl,
                dump_rdata(&r.data)
            );
        }
    }
    if let Some(e) = &m.edns {
        println!(
            "{tag} edns payload={} ver={} do={}",
            e.max_payload(),
            e.version(),
            e.flags().dnssec_ok
        );
    }
}

/// Encode → print the byte fingerprint → decode back → dump the fields → roundtrip boolean
fn roundtrip(tag: &str, msg: &Message) {
    let bytes = msg.to_vec().unwrap();
    println!("{tag} encoded len={} fnv={:016x}", bytes.len(), fnv1a(&bytes));
    println!("{tag} hex={}", hex(&bytes));
    let back = Message::from_vec(&bytes).unwrap();
    dump_message(tag, &back);
    println!("{tag} roundtrip={}", *msg == back);
}

fn main() {
    // ① Standard query: RD plus two questions (A / AAAA)
    let mut q = Message::new(0xBEEF, MessageType::Query, OpCode::Query);
    q.metadata.recursion_desired = true;
    q.add_query(Query::query(Name::from_ascii("www.example.com.").unwrap(), RecordType::A));
    q.add_query(Query::query(Name::from_ascii("ipv6.example.com.").unwrap(), RecordType::AAAA));
    roundtrip("1", &q);

    // ② Response: nine RData kinds + glue + EDNS; shared suffixes trigger compression
    let mut r = Message::new(0x1234, MessageType::Response, OpCode::Query);
    r.metadata.recursion_desired = true;
    r.metadata.recursion_available = true;
    r.metadata.authoritative = true;
    r.metadata.response_code = ResponseCode::NoError;
    let www = Name::from_ascii("www.example.com.").unwrap();
    r.add_query(Query::query(www.clone(), RecordType::A));
    r.add_answer(Record::from_rdata(www.clone(), 300, RData::A(A(Ipv4Addr::new(93, 184, 216, 34)))));
    r.add_answer(Record::from_rdata(
        www.clone(),
        u32::MAX,
        RData::AAAA(AAAA(Ipv6Addr::new(0x2606, 0x2800, 0x220, 0x1, 0x248, 0x1893, 0x25c8, 0x1946))),
    ));
    r.add_answer(Record::from_rdata(
        Name::from_ascii("alias.example.com.").unwrap(),
        0,
        RData::CNAME(CNAME(www.clone())),
    ));
    r.add_answer(Record::from_rdata(
        Name::from_ascii("example.com.").unwrap(),
        3600,
        RData::MX(MX::new(10, Name::from_ascii("mail.example.com.").unwrap())),
    ));
    r.add_answer(Record::from_rdata(
        Name::from_ascii("example.com.").unwrap(),
        7200,
        RData::TXT(TXT::new(vec!["v=spf1 -all".to_string(), String::new()])),
    ));
    r.add_answer(Record::from_rdata(
        Name::from_ascii("_sip._tcp.example.com.").unwrap(),
        60,
        RData::SRV(SRV::new(1, 5, 5060, Name::from_ascii("sip.example.com.").unwrap())),
    ));
    r.add_answer(Record::from_rdata(
        Name::from_ascii("example.com.").unwrap(),
        86400,
        RData::NS(NS(Name::from_ascii("ns1.example.com.").unwrap())),
    ));
    r.add_answer(Record::from_rdata(
        Name::from_ascii("34.216.184.93.in-addr.arpa.").unwrap(),
        300,
        RData::PTR(PTR(www.clone())),
    ));
    r.add_authority(Record::from_rdata(
        Name::from_ascii("example.com.").unwrap(),
        3600,
        RData::SOA(SOA::new(
            Name::from_ascii("ns1.example.com.").unwrap(),
            Name::from_ascii("hostmaster.example.com.").unwrap(),
            2_026_071_500,
            7200,
            3600,
            1_209_600,
            300,
        )),
    ));
    r.add_additional(Record::from_rdata(
        Name::from_ascii("ns1.example.com.").unwrap(),
        86400,
        RData::A(A(Ipv4Addr::new(192, 0, 2, 53))),
    ));
    let mut edns = Edns::new();
    edns.set_max_payload(1232);
    edns.set_dnssec_ok(true);
    r.set_edns(edns);
    roundtrip("2", &r);

    // ③ NXDomain error response: SOA in authority, no answer
    let mut nx = Message::new(0x0002, MessageType::Response, OpCode::Query);
    nx.metadata.recursion_desired = true;
    nx.metadata.recursion_available = true;
    nx.metadata.response_code = ResponseCode::NXDomain;
    nx.add_query(Query::query(Name::from_ascii("nope.example.com.").unwrap(), RecordType::A));
    nx.add_authority(Record::from_rdata(
        Name::from_ascii("example.com.").unwrap(),
        300,
        RData::SOA(SOA::new(
            Name::from_ascii("ns1.example.com.").unwrap(),
            Name::from_ascii("hostmaster.example.com.").unwrap(),
            1,
            7200,
            3600,
            1_209_600,
            300,
        )),
    ));
    roundtrip("3", &nx);

    // ④ truncate: clears the sections, sets tc, keeps queries/edns
    let t = r.truncate();
    println!("4 truncated tc={} an={} ns={} ar={} q={}", t.metadata.truncation, t.answers.len(), t.authorities.len(), t.additionals.len(), t.queries.len());
    roundtrip("4", &t);

    // ⑤ Name details: root, case preservation, idna (punycode), label count, query class
    let root = Name::root();
    println!("5 root ascii={:?} labels={} len={}", root.to_ascii(), root.num_labels(), root.len());
    let mixed = Name::from_ascii("WWW.Example.COM.").unwrap();
    println!("5 mixed ascii={} labels={}", mixed.to_ascii(), mixed.num_labels());
    let uni = Name::from_utf8("bücher.example.").unwrap();
    println!("5 idna utf8={} ascii={}", uni.to_utf8(), uni.to_ascii());
    let mut qc = Query::query(root.clone(), RecordType::NS);
    qc.set_query_class(DNSClass::CH);
    let mut msg5 = Message::new(0x00FF, MessageType::Query, OpCode::Query);
    msg5.add_query(qc);
    roundtrip("5", &msg5);

    // ⑥ decode error paths (the printed text is deterministic)
    let e1 = Message::from_vec(&[]).unwrap_err();
    println!("6 empty-input err: {e1}");
    let full = r.to_vec().unwrap();
    let e2 = Message::from_vec(&full[..full.len() / 2]).unwrap_err();
    println!("6 truncated-input err: {e2}");
    // Compression pointer pointing at itself (idx=12, ptr=12)
    let mut bad = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 0, 0, 0, 0, 0];
    bad.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1]);
    let e3 = Message::from_vec(&bad).unwrap_err();
    println!("6 self-pointer err: {e3}");
    let long_label = format!("{}.example.com.", "a".repeat(64));
    let e4 = Name::from_ascii(&long_label).unwrap_err();
    println!("6 long-label err: {e4}");
}
