/// Product name used in HTTP User-Agent and the BEP-0010 handshake `v` field.
pub const CLIENT_NAME: &str = "bitrev";

/// Azureus-style two-character client code (BEP-0020).
pub const CLIENT_CODE: [u8; 2] = *b"BR";

pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// HTTP User-Agent, e.g. `bitrev/0.1.0`.
pub fn user_agent() -> String {
    format!("{CLIENT_NAME}/{CLIENT_VERSION}")
}

/// BEP-0010 handshake `v` value, e.g. `bitrev 0.1.0`.
pub fn extension_version() -> String {
    format!("{CLIENT_NAME} {CLIENT_VERSION}")
}

/// Azureus-style 8-byte peer id prefix: `-BRxyzw-`.
pub fn peer_id_prefix() -> [u8; 8] {
    let version = azureus_version(CLIENT_VERSION);
    [
        b'-',
        CLIENT_CODE[0],
        CLIENT_CODE[1],
        version[0],
        version[1],
        version[2],
        version[3],
        b'-',
    ]
}

/// Azureus xyzw: 1-digit major, minor, patch, then 0. 0.1.0 -> 0100.
pub fn azureus_version(version: &str) -> [u8; 4] {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = core.split('.');
    let major = parse_component(parts.next());
    let minor = parse_component(parts.next());
    let patch = parse_component(parts.next());
    [
        b'0' + (major % 10) as u8,
        b'0' + (minor % 10) as u8,
        b'0' + (patch % 10) as u8,
        b'0',
    ]
}

fn parse_component(part: Option<&str>) -> u32 {
    part.unwrap_or("0").parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_strings_share_name_and_version() {
        assert_eq!(user_agent(), format!("{CLIENT_NAME}/{CLIENT_VERSION}"));
        assert_eq!(
            extension_version(),
            format!("{CLIENT_NAME} {CLIENT_VERSION}")
        );
        assert_eq!(&peer_id_prefix(), b"-BR0100-");
    }

    #[test]
    fn azureus_version_encodes_major_minor_patch() {
        assert_eq!(azureus_version("0.1.0"), *b"0100");
        assert_eq!(azureus_version("1.2.3"), *b"1230");
        assert_eq!(azureus_version("0.1.0-alpha"), *b"0100");
    }
}
