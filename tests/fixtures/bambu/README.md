# Bambu Lab CA certificates (test fixtures)

- `bbl_ca2_rsa_cross.der`: `BBL CA2 RSA`, cross-signed by `BBL CA`.
- `bbl_ca2_ecc_cross.der`: `BBL CA2 ECC`, cross-signed by `BBL CA`.

Both are public CA certificates, copied from ha-bambulab
(https://github.com/greghesp/ha-bambulab),
`custom_components/bambu_lab/pybambu/certs/bambu.cert` at commit `cd67ed9`
(2026-09-11), where they are the third and fourth certificates. The tests use
them to show that certificates `BBL CA` signed that are not printer leaves are
refused by the serial binding.

These certificates belong to Bambu Lab. They are included only to test printer
verification and are not covered by this project's MIT/Apache-2.0 license.
No printer certificate is ever committed: a printer leaf's CN is its serial.
