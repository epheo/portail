//! PROXY protocol v2, the receiving half: a connection from a trusted
//! source opens with a header naming the client it forwards.
//! https://www.haproxy.org/download/3.0/doc/proxy-protocol.txt

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::io::{AsyncRead, AsyncReadExt};

const SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";

/// What a trusted source's header says of its connection.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Header {
    /// PROXY: the client the source forwards.
    Proxy(SocketAddr),
    /// LOCAL: the source speaks for itself, a health check: the
    /// connection's own address stands.
    Local,
}

fn malformed(why: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why)
}

/// Read exactly the header and nothing after it: a TLS hello or request
/// head that follows stays on the stream. Anything but a v2 header for a
/// TCP client, or LOCAL, is an error: the source is trusted to send one,
/// so the transport address never stands in for it.
pub(crate) async fn read_header<R: AsyncRead + Unpin>(stream: &mut R) -> io::Result<Header> {
    let mut fixed = [0u8; 16];
    stream.read_exact(&mut fixed).await?;
    if fixed[..12] != SIGNATURE {
        return Err(malformed("no PROXY protocol v2 signature"));
    }
    let (version, command) = (fixed[12] >> 4, fixed[12] & 0x0f);
    if version != 2 {
        return Err(malformed("not PROXY protocol version 2"));
    }
    let family = fixed[13];
    let len = u16::from_be_bytes([fixed[14], fixed[15]]);
    let block = match (command, family) {
        // LOCAL: the address block, if any, means nothing.
        (0x0, _) => 0,
        // TCP over IPv4, TCP over IPv6.
        (0x1, 0x11) => 12,
        (0x1, 0x21) => 36,
        (0x1, _) => return Err(malformed("PROXY for a client that is not TCP over IP")),
        _ => return Err(malformed("unknown PROXY protocol command")),
    };
    if usize::from(len) < block {
        return Err(malformed("address block shorter than its family"));
    }
    let mut addresses = [0u8; 36];
    stream.read_exact(&mut addresses[..block]).await?;
    // TLVs: nothing here reads them.
    let rest = u64::from(len) - block as u64;
    let skipped = tokio::io::copy(&mut (&mut *stream).take(rest), &mut tokio::io::sink()).await?;
    if skipped != rest {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(match block {
        12 => {
            let ip = Ipv4Addr::from(<[u8; 4]>::try_from(&addresses[..4]).unwrap());
            let port = u16::from_be_bytes([addresses[8], addresses[9]]);
            Header::Proxy(SocketAddr::new(ip.into(), port))
        }
        36 => {
            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&addresses[..16]).unwrap());
            let port = u16::from_be_bytes([addresses[32], addresses[33]]);
            Header::Proxy(SocketAddr::new(ip.into(), port))
        }
        _ => Header::Local,
    })
}

/// The v2 header a connection to a Gateway backend opens with: PROXY for
/// `client`, which reached this data plane at `local`. No TLVs.
pub(crate) fn header(client: SocketAddr, local: SocketAddr) -> Vec<u8> {
    let (src, dst) = (client.ip().to_canonical(), local.ip().to_canonical());
    let mut h = Vec::with_capacity(52);
    h.extend_from_slice(&SIGNATURE);
    h.push(0x21);
    match (src, dst) {
        (IpAddr::V4(src), IpAddr::V4(dst)) => {
            h.extend_from_slice(&[0x11, 0, 12]);
            h.extend_from_slice(&src.octets());
            h.extend_from_slice(&dst.octets());
        }
        // One family per header: an IPv4 end goes IPv4-mapped.
        _ => {
            let v6 = |ip: IpAddr| match ip {
                IpAddr::V4(v4) => v4.to_ipv6_mapped(),
                IpAddr::V6(v6) => v6,
            };
            h.extend_from_slice(&[0x21, 0, 36]);
            h.extend_from_slice(&v6(src).octets());
            h.extend_from_slice(&v6(dst).octets());
        }
    }
    h.extend_from_slice(&client.port().to_be_bytes());
    h.extend_from_slice(&local.port().to_be_bytes());
    h
}

/// An IPv4 or IPv6 network, host bits clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Result<Self, String> {
        let (ip, prefix) = s
            .split_once('/')
            .ok_or_else(|| format!("{s:?} is not a CIDR: no /prefix"))?;
        let network: IpAddr = ip
            .parse()
            .map_err(|_| format!("{s:?} is not a CIDR: {ip:?} is not an IP address"))?;
        let bits = if network.is_ipv4() { 32 } else { 128 };
        let prefix: u8 = prefix
            .parse()
            .ok()
            .filter(|p| *p <= bits)
            .ok_or_else(|| format!("{s:?} is not a CIDR: prefix is not 0..={bits}"))?;
        // A typo such as 10.0.0.5/8 for /32 would trust a whole network.
        if bits_of(network) & !mask(bits, prefix) != 0 {
            return Err(format!(
                "{s:?} has host bits set: write its network address"
            ));
        }
        Ok(Self { network, prefix })
    }

    /// An IPv4 client of a dual-stack socket arrives IPv4-mapped.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        if ip.is_ipv4() != self.network.is_ipv4() {
            return false;
        }
        let m = mask(if ip.is_ipv4() { 32 } else { 128 }, self.prefix);
        bits_of(ip) & m == bits_of(self.network)
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

fn bits_of(ip: IpAddr) -> u128 {
    match ip {
        IpAddr::V4(v4) => u32::from(v4).into(),
        IpAddr::V6(v6) => v6.into(),
    }
}

fn mask(bits: u8, prefix: u8) -> u128 {
    let width = if bits == 32 {
        u32::MAX as u128
    } else {
        u128::MAX
    };
    if prefix == 0 {
        0
    } else {
        width & (width << (bits - prefix))
    }
}

/// The sources whose connections must open with a PROXY v2 header. Empty:
/// none, and no connection is read for one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrustedSources(Vec<Cidr>);

impl TrustedSources {
    pub fn new(cidrs: Vec<Cidr>) -> Self {
        Self(cidrs)
    }

    #[inline]
    pub fn trusts(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|c| c.contains(ip))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2(command: u8, family: u8, addresses: &[u8], tlvs: &[u8]) -> Vec<u8> {
        let mut h = SIGNATURE.to_vec();
        h.push(0x20 | command);
        h.push(family);
        h.extend(((addresses.len() + tlvs.len()) as u16).to_be_bytes());
        h.extend(addresses);
        h.extend(tlvs);
        h
    }

    fn tcp4(src: [u8; 4], port: u16) -> Vec<u8> {
        let mut a = src.to_vec();
        a.extend([192, 0, 2, 1]);
        a.extend(port.to_be_bytes());
        a.extend(443u16.to_be_bytes());
        a
    }

    async fn read(bytes: &[u8]) -> (io::Result<Header>, Vec<u8>) {
        let mut stream = bytes;
        let header = read_header(&mut stream).await;
        (header, stream.to_vec())
    }

    #[tokio::test]
    async fn a_tcp4_header_names_its_client_and_leaves_what_follows() {
        let mut bytes = v2(1, 0x11, &tcp4([203, 0, 113, 7], 50000), &[]);
        bytes.extend(b"GET / HTTP/1.1\r\n");
        let (header, rest) = read(&bytes).await;
        assert_eq!(
            header.unwrap(),
            Header::Proxy("203.0.113.7:50000".parse().unwrap())
        );
        assert_eq!(rest, b"GET / HTTP/1.1\r\n");
    }

    #[tokio::test]
    async fn a_tcp6_header_names_its_client() {
        let src: Ipv6Addr = "2001:db8::7".parse().unwrap();
        let mut a = src.octets().to_vec();
        a.extend(Ipv6Addr::LOCALHOST.octets());
        a.extend(50001u16.to_be_bytes());
        a.extend(443u16.to_be_bytes());
        let (header, _) = read(&v2(1, 0x21, &a, &[])).await;
        assert_eq!(
            header.unwrap(),
            Header::Proxy("[2001:db8::7]:50001".parse().unwrap())
        );
    }

    #[tokio::test]
    async fn tlvs_are_skipped_whole() {
        let tlv = [0x04, 0x00, 0x03, b'a', b'b', b'c'];
        let mut bytes = v2(1, 0x11, &tcp4([198, 51, 100, 1], 1), &tlv);
        bytes.push(0x16);
        let (header, rest) = read(&bytes).await;
        assert!(matches!(header.unwrap(), Header::Proxy(_)));
        assert_eq!(rest, [0x16]);
    }

    #[tokio::test]
    async fn local_keeps_the_connection_address_whatever_its_block() {
        for (family, block) in [(0x00, vec![]), (0x11, tcp4([10, 0, 0, 1], 1))] {
            let mut bytes = v2(0, family, &block, &[]);
            bytes.push(b'G');
            let (header, rest) = read(&bytes).await;
            assert_eq!(header.unwrap(), Header::Local);
            assert_eq!(rest, b"G");
        }
    }

    #[tokio::test]
    async fn anything_else_is_refused() {
        let mut v1 = b"PROXY TCP4 203.0.113.7 192.0.2.1 50000 443\r\n".to_vec();
        v1.resize(32, 0);
        let mut version_1 = v2(1, 0x11, &tcp4([1, 1, 1, 1], 1), &[]);
        version_1[12] = 0x11;
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("a request", b"GET / HTTP/1.1\r\nHost: a\r\n\r\n".to_vec()),
            ("v1 text", v1),
            ("version 1 in a v2 header", version_1),
            ("unknown command", v2(2, 0x11, &tcp4([1, 1, 1, 1], 1), &[])),
            ("UDP client", v2(1, 0x12, &tcp4([1, 1, 1, 1], 1), &[])),
            ("unix client", v2(1, 0x31, &[0; 216], &[])),
            ("PROXY with no family", v2(1, 0x00, &[], &[])),
            ("short block", v2(1, 0x21, &tcp4([1, 1, 1, 1], 1), &[])),
        ];
        for (case, bytes) in cases {
            assert!(read(&bytes).await.0.is_err(), "{case}");
        }
        let mut cut = v2(1, 0x11, &tcp4([1, 1, 1, 1], 1), &[0x04, 0x00, 0x09]);
        cut.truncate(cut.len() - 1);
        assert!(read(&cut).await.0.is_err(), "TLVs cut short");
        assert!(read(&SIGNATURE).await.0.is_err(), "fixed part cut short");
    }

    #[tokio::test]
    async fn a_written_header_reads_back_as_its_client() {
        let cases = [
            ("203.0.113.7:50000", "192.0.2.1:443", "203.0.113.7:50000"),
            ("[2001:db8::7]:1", "[2001:db8::1]:80", "[2001:db8::7]:1"),
            (
                "203.0.113.7:2",
                "[2001:db8::1]:80",
                "[::ffff:203.0.113.7]:2",
            ),
            (
                "[::ffff:203.0.113.7]:3",
                "[::ffff:192.0.2.1]:443",
                "203.0.113.7:3",
            ),
        ];
        for (client, local, want) in cases {
            let mut bytes = header(client.parse().unwrap(), local.parse().unwrap());
            bytes.push(0x16);
            let (read_back, rest) = read(&bytes).await;
            assert_eq!(
                read_back.unwrap(),
                Header::Proxy(want.parse().unwrap()),
                "{client}"
            );
            assert_eq!(rest, [0x16]);
        }
        let v4 = header(
            "203.0.113.7:1".parse().unwrap(),
            "192.0.2.1:2".parse().unwrap(),
        );
        assert_eq!(v4.len(), 28);
        assert_eq!(&v4[12..16], &[0x21, 0x11, 0, 12]);
    }

    #[test]
    fn cidrs_parse_networks_only() {
        for good in [
            "10.0.0.0/8",
            "10.1.2.3/32",
            "0.0.0.0/0",
            "2001:db8::/32",
            "::1/128",
        ] {
            assert_eq!(Cidr::parse(good).unwrap().to_string(), good);
        }
        for bad in [
            "10.0.0.5/8",
            "10.0.0.0",
            "10.0.0.0/33",
            "2001:db8::/129",
            "x/8",
            "10.0.0.0/",
        ] {
            assert!(Cidr::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_source_is_trusted_inside_a_cidr_of_its_family() {
        let trusted = TrustedSources::new(vec![
            Cidr::parse("10.250.0.0/16").unwrap(),
            Cidr::parse("2001:db8::/64").unwrap(),
        ]);
        for (ip, want) in [
            ("10.250.3.4", true),
            ("::ffff:10.250.3.4", true),
            ("10.251.0.1", false),
            ("2001:db8::9", true),
            ("2001:db8:0:1::9", false),
            ("::10.250.3.4", false),
        ] {
            assert_eq!(trusted.trusts(ip.parse().unwrap()), want, "{ip}");
        }
        assert!(!TrustedSources::default().trusts("10.250.3.4".parse().unwrap()));
        assert!(TrustedSources::new(vec![Cidr::parse("0.0.0.0/0").unwrap()])
            .trusts("192.0.2.1".parse().unwrap()));
    }
}
