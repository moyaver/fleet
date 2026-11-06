// SPDX-License-Identifier: AGPL-3.0-only
//! The client's IP address, for the per-IP limits (research 03 §11; issue #35).
//!
//! Behind a reverse proxy the socket peer is the proxy, and `X-Forwarded-For` names the client.
//! The header is client-controlled, so it counts only when the peer is one of `trusted_proxies`.
//! Then the client is the rightmost entry that is not itself a trusted proxy: the proxies append
//! what they saw, so everything to its left was supplied by the client. Otherwise the peer is the
//! client, and a server exposed directly ignores the header.

use std::net::{IpAddr, SocketAddr};

use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::{HeaderMap, HeaderValue, StatusCode, request::Parts},
    response::Response,
};
use ipnet::IpNet;

use crate::{api::error, state::AppState};

/// The header a proxy appends the client's address to. (`http` has no constant for it.)
const X_FORWARDED_FOR: &str = "x-forwarded-for";

/// The client's address for this request. Resolved once, by the extractor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClientIp(pub(crate) IpAddr);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        // The listener serves with `into_make_service_with_connect_info`; without it the peer
        // is unknown and the request is refused rather than counted against a made-up address.
        let ConnectInfo(peer) = ConnectInfo::<SocketAddr>::from_request_parts(parts, state)
            .await
            .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
        Ok(Self(resolve(
            peer.ip(),
            &parts.headers,
            &state.config.trusted_proxies,
        )))
    }
}

/// The client's address: the peer, unless the peer is a trusted proxy. Then it is the rightmost
/// `X-Forwarded-For` entry that is not a trusted proxy. Across several header lines the entries
/// read as one list, left to right.
///
/// The peer is also the answer when the header is missing, when an entry that has to be read is
/// not an address, or when every entry is a trusted proxy. Entries to the left of the answer are
/// never read, so garbage there cannot change it.
pub(crate) fn resolve(peer: IpAddr, headers: &HeaderMap, trusted: &[IpNet]) -> IpAddr {
    let peer = peer.to_canonical();
    if !is_trusted(peer, trusted) {
        return peer;
    }
    let Ok(lines) = headers
        .get_all(X_FORWARDED_FOR)
        .iter()
        .map(HeaderValue::to_str)
        .collect::<Result<Vec<&str>, _>>()
    else {
        return peer;
    };
    for entry in lines.iter().flat_map(|line| line.split(',')).rev() {
        let Some(ip) = parse_entry(entry) else {
            return peer;
        };
        let ip = ip.to_canonical();
        if !is_trusted(ip, trusted) {
            return ip;
        }
    }
    peer
}

fn is_trusted(ip: IpAddr, trusted: &[IpNet]) -> bool {
    trusted.iter().any(|net| net.contains(&ip))
}

/// One `X-Forwarded-For` entry: an address, `address:port`, or a bracketed IPv6 address.
fn parse_entry(entry: &str) -> Option<IpAddr> {
    let entry = entry.trim();
    entry
        .parse::<IpAddr>()
        .ok()
        .or_else(|| entry.parse::<SocketAddr>().ok().map(|a| a.ip()))
        .or_else(|| {
            entry
                .strip_prefix('[')?
                .strip_suffix(']')?
                .parse::<IpAddr>()
                .ok()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn nets(list: &[&str]) -> Vec<IpNet> {
        list.iter().map(|n| n.parse().unwrap()).collect()
    }

    fn xff(lines: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for line in lines {
            h.append(X_FORWARDED_FOR, HeaderValue::from_str(line).unwrap());
        }
        h
    }

    #[test]
    fn a_trusted_proxy_hands_over_the_rightmost_untrusted_entry() {
        let trusted = nets(&["10.0.0.0/8"]);
        // The client sent `6.6.6.6` itself; the proxy appended the address it saw.
        let h = xff(&["6.6.6.6, 203.0.113.9"]);
        assert_eq!(resolve(ip("10.0.0.2"), &h, &trusted), ip("203.0.113.9"));
        // A chain of internal proxies: the trusted hops are skipped.
        let chain = xff(&["203.0.113.9, 10.0.0.5, 10.0.0.6"]);
        assert_eq!(resolve(ip("10.0.0.2"), &chain, &trusted), ip("203.0.113.9"));
    }

    #[test]
    fn an_untrusted_peer_is_the_client_whatever_the_header_says() {
        let trusted = nets(&["10.0.0.0/8"]);
        let h = xff(&["203.0.113.9"]);
        assert_eq!(
            resolve(ip("198.51.100.7"), &h, &trusted),
            ip("198.51.100.7")
        );
        assert_eq!(
            resolve(ip("198.51.100.7"), &h, &[]),
            ip("198.51.100.7"),
            "no trusted proxies: the header is ignored"
        );
    }

    #[test]
    fn a_missing_or_garbled_header_falls_back_to_the_peer() {
        let trusted = nets(&["10.0.0.0/8"]);
        let peer = ip("10.0.0.2");
        assert_eq!(resolve(peer, &HeaderMap::new(), &trusted), peer);
        for garbled in [
            "",
            "nonsense",
            "203.0.113.9, nonsense",
            "203.0.113.9,",
            "300.1.1.1",
        ] {
            assert_eq!(
                resolve(peer, &xff(&[garbled]), &trusted),
                peer,
                "{garbled:?}"
            );
        }
        let not_utf8 = {
            let mut h = HeaderMap::new();
            h.append(
                X_FORWARDED_FOR,
                HeaderValue::from_bytes(b"\xff\xfe").unwrap(),
            );
            h
        };
        assert_eq!(resolve(peer, &not_utf8, &trusted), peer);
    }

    #[test]
    fn when_every_entry_is_trusted_the_peer_is_the_client() {
        let trusted = nets(&["10.0.0.0/8"]);
        assert_eq!(
            resolve(ip("10.0.0.2"), &xff(&["10.0.0.9, 10.0.0.8"]), &trusted),
            ip("10.0.0.2")
        );
    }

    #[test]
    fn garbage_to_the_left_of_the_answer_is_never_read() {
        let trusted = nets(&["10.0.0.0/8"]);
        let h = xff(&["nonsense, 203.0.113.9"]);
        assert_eq!(resolve(ip("10.0.0.2"), &h, &trusted), ip("203.0.113.9"));
    }

    #[test]
    fn spaces_ports_and_several_header_lines_are_read() {
        let trusted = nets(&["10.0.0.0/8"]);
        let peer = ip("10.0.0.2");
        assert_eq!(
            resolve(
                peer,
                &xff(&["  6.6.6.6 ,  203.0.113.9:4711 ,10.0.0.5  "]),
                &trusted
            ),
            ip("203.0.113.9")
        );
        assert_eq!(
            resolve(
                peer,
                &xff(&["6.6.6.6", "203.0.113.9", "10.0.0.5"]),
                &trusted
            ),
            ip("203.0.113.9"),
            "lines read as one list"
        );
    }

    #[test]
    fn ipv6_clients_and_proxies() {
        let trusted = nets(&["2001:db8:ffff::/48"]);
        let peer = ip("2001:db8:ffff::2");
        assert_eq!(
            resolve(peer, &xff(&["2001:4860::8888"]), &trusted),
            ip("2001:4860::8888")
        );
        assert_eq!(
            resolve(peer, &xff(&["[2001:4860::8888]:443"]), &trusted),
            ip("2001:4860::8888")
        );
        assert_eq!(
            resolve(peer, &xff(&["[2001:4860::8888]"]), &trusted),
            ip("2001:4860::8888")
        );
        // An IPv4 client behind a v6 proxy, and a v4-mapped v6 peer matched by its v4 block.
        let v4 = nets(&["10.0.0.0/8"]);
        assert_eq!(
            resolve(ip("::ffff:10.0.0.2"), &xff(&["203.0.113.9"]), &v4),
            ip("203.0.113.9"),
            "a mapped peer is canonical"
        );
        assert_eq!(
            resolve(ip("2001:db8:ffff::2"), &xff(&["203.0.113.9"]), &v4),
            ip("2001:db8:ffff::2"),
            "a v6 peer is not in a v4 block"
        );
    }
}
