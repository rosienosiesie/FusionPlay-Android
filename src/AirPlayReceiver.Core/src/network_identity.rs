use sha2::{Digest, Sha256};
use std::net::Ipv6Addr;

pub(crate) fn normalize_hardware_address(value: &str) -> Option<String> {
    let compact = value
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_uppercase();
    if compact.len() != 12
        || compact == "000000000000"
        || compact == "FFFFFFFFFFFF"
        || compact == "020000000000"
    {
        return None;
    }
    let first_octet = u8::from_str_radix(&compact[..2], 16).ok()?;
    if first_octet & 1 != 0 {
        return None;
    }
    Some(compact)
}

pub(crate) fn stable_local_hardware_address(stable_seed: &str) -> String {
    let digest = Sha256::digest(format!("fusionplay-miplay-hardware:{stable_seed}").as_bytes());
    let mut address = [0u8; 6];
    address.copy_from_slice(&digest[..6]);
    // A deterministic locally administered unicast address keeps Xiaomi's
    // two discovery transports on one identity when Android hides the
    // physical interface address from third-party applications.
    address[0] = (address[0] | 0x02) & 0xfe;
    hex::encode_upper(address)
}

pub(crate) fn hardware_address_from_ipv6_eui64(address: Ipv6Addr) -> Option<String> {
    let octets = address.octets();
    let interface_id = &octets[8..];
    if interface_id[3] != 0xff || interface_id[4] != 0xfe {
        return None;
    }
    let hardware_address = [
        interface_id[0] ^ 0x02,
        interface_id[1],
        interface_id[2],
        interface_id[5],
        interface_id[6],
        interface_id[7],
    ];
    normalize_hardware_address(&hex::encode_upper(hardware_address))
}

pub(crate) fn select_hardware_address(
    sysfs_address: Option<&str>,
    eui64_address: Option<&str>,
    host_address: Option<&str>,
    stable_seed: &str,
) -> (String, &'static str) {
    sysfs_address
        .and_then(normalize_hardware_address)
        .map(|address| (address, "android_sysfs"))
        .or_else(|| {
            eui64_address
                .and_then(normalize_hardware_address)
                .map(|address| (address, "android_ipv6_eui64"))
        })
        .or_else(|| {
            host_address
                .and_then(normalize_hardware_address)
                .map(|address| (address, "host_adapter"))
        })
        .unwrap_or_else(|| {
            (
                stable_local_hardware_address(stable_seed),
                "persistent_local_fallback",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{
        hardware_address_from_ipv6_eui64, normalize_hardware_address, select_hardware_address,
        stable_local_hardware_address,
    };
    use std::net::Ipv6Addr;

    #[test]
    fn hardware_addresses_are_normalized_and_privacy_placeholders_are_rejected() {
        assert_eq!(
            normalize_hardware_address("a0:36-bc:25-05:43"),
            Some("A036BC250543".to_owned())
        );
        assert_eq!(normalize_hardware_address("02:00:00:00:00:00"), None);
        assert_eq!(normalize_hardware_address("FF:FF:FF:FF:FF:FF"), None);
        assert_eq!(normalize_hardware_address("01:23:45:67:89:AB"), None);
        assert_eq!(normalize_hardware_address("BC250543"), None);
    }

    #[test]
    fn fallback_address_is_stable_locally_administered_and_unicast() {
        let first = stable_local_hardware_address("idm-test");
        let second = stable_local_hardware_address("idm-test");
        assert_eq!(first, second);
        assert_eq!(first.len(), 12);
        let first_octet = u8::from_str_radix(&first[..2], 16).unwrap();
        assert_eq!(first_octet & 0x02, 0x02);
        assert_eq!(first_octet & 0x01, 0);
        assert_ne!(first, stable_local_hardware_address("another-idm"));
    }

    #[test]
    fn physical_address_is_recovered_from_android_ipv6_eui64() {
        let address: Ipv6Addr = "fe80::828:ffff:fe31:bdfd".parse().unwrap();
        assert_eq!(
            hardware_address_from_ipv6_eui64(address),
            Some("0A28FF31BDFD".to_owned())
        );
        assert_eq!(
            hardware_address_from_ipv6_eui64("fe80::2906:9478:478d:b320".parse().unwrap()),
            None
        );
    }

    #[test]
    fn physical_interface_identity_wins_over_host_privacy_address() {
        let (address, source) =
            select_hardware_address(None, Some("0A28FF31BDFD"), Some("2277AE89DC25"), "idm-test");
        assert_eq!(address, "0A28FF31BDFD");
        assert_eq!(source, "android_ipv6_eui64");
    }
}
