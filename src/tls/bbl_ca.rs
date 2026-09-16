//! Bambu Lab's `BBL CA` certificate: the trust anchor of the printer
//! certificate verifier (design doc 5.3).
//!
//! Source: ha-bambulab (https://github.com/greghesp/ha-bambulab), file
//! `custom_components/bambu_lab/pybambu/certs/bambu.cert` at commit
//! `cd67ed9` of 2026-09-11, where it is the fifth certificate. It is
//! byte-identical to the fifth certificate of Bambu Studio's
//! `resources/cert/printer.cer` and to the CA certificate the printers send
//! after their leaf.
//!
//! Subject and issuer `C=CN, O=BBL Technologies Co., Ltd, CN=BBL CA`;
//! X.509 v3, CA:TRUE, RSA-2048, valid 2022-04-04 to 2032-04-01; DER 873
//! bytes, SHA-256
//! 030bca81cece18b7eff3cfd2b75d09d3efca893bc069609e37fa04257fe4d840.
//!
//! This certificate belongs to Bambu Lab. It is included only to verify
//! printers and is not covered by this project's MIT/Apache-2.0 licence.

/// DER of `BBL CA`, stored as `assets/bbl_ca.der`.
pub const BBL_CA_DER: &[u8] = include_bytes!("../../assets/bbl_ca.der");

#[cfg(test)]
mod tests {
    use super::BBL_CA_DER;

    /// SHA-256 of the DER as copied from ha-bambulab `cd67ed9`.
    const BBL_CA_DER_SHA256: &str =
        "030bca81cece18b7eff3cfd2b75d09d3efca893bc069609e37fa04257fe4d840";

    #[test]
    fn bbl_ca_der_sha256_matches_constant() {
        let digest = ring::digest::digest(&ring::digest::SHA256, BBL_CA_DER);
        let hex: String =
            digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(BBL_CA_DER.len(), 873);
        assert_eq!(hex, BBL_CA_DER_SHA256);
    }
}
