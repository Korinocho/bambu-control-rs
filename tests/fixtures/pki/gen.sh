#!/usr/bin/env bash
# Regenerates the synthetic test PKI used by src/tls/tests.rs (design doc 5.3,
# "Fixtures"). Every certificate and key here is made up for the tests: no
# printer certificate, serial or key is ever read or written by this script.
#
# Outputs are DER only (certificates *.der, PKCS#8 keys *.pk8.der); PEM files
# live in a temporary directory that is removed at the end.
#
# OpenSSL 3.5 produces an X.509 v1 certificate only with
# `req -new -x509 -x509v1 -CA ... -CAkey ...` and a minimal -config without
# x509_extensions; the default config gives v3.
set -euo pipefail
OPENSSL="${OPENSSL:-openssl}"
OUT="$(cd "$(dirname "$0")" && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
cd "$TMP"

# Shaped like a printer serial (15 characters), but not one.
SERIAL="01P00Z9X8W7V6U5"
TEST_CA_DN="/C=CN/O=Test CA Org/CN=Test CA"
# Byte-identical to the subject of Bambu's BBL CA (checked by the tests).
BBL_CA_DN="/C=CN/O=BBL Technologies Co., Ltd/CN=BBL CA"
DEVICE_CA_DN="/C=CN/O=BBL Technologies Co. Ltd/CN=BBL Device CA O1C2-V2"

printf '[req]\ndistinguished_name = dn\nprompt = no\nstring_mask = utf8only\nutf8 = yes\n[dn]\nCN = x\n' > v1.cnf

key_rsa() {
  "$OPENSSL" genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
      -out "$1.key.pem" 2>/dev/null
  "$OPENSSL" pkcs8 -topk8 -nocrypt -in "$1.key.pem" -outform DER \
      -out "$OUT/$1.pk8.der"
}
# ca <name> <subject> [extra req options]
ca() {
  local name="$1" subj="$2"; shift 2
  "$OPENSSL" req -new -x509 -key "$name.key.pem" -subj "$subj" -sha256 "$@" \
      -out "$name.pem"
  "$OPENSSL" x509 -in "$name.pem" -outform DER -out "$OUT/$name.der"
}
# v1 <out> <subject> <key> [extra req options: -CA/-CAkey, dates, digest]
v1() {
  local out="$1" subj="$2" key="$3"; shift 3
  "$OPENSSL" req -new -x509 -x509v1 -config v1.cnf -subj "$subj" \
      -key "$key.key.pem" -outform DER -out "$OUT/$out" "$@" 2>&1 \
      | { grep -v '^Warning' || true; }
}

key_rsa ca
key_rsa leaf
key_rsa other
key_rsa evil_ca
key_rsa evil_bblca
key_rsa devca
key_rsa window_ca

ca ca "$TEST_CA_DN" -days 3650
# attacker CAs: the Test CA name, and the BBL CA name, each with its own key
ca evil_ca "$TEST_CA_DN" -days 3650
ca evil_bblca "$BBL_CA_DN" -days 3650
# a CA named like the H2C device CA of Bambu's V2 hierarchy
ca devca "$DEVICE_CA_DN" -days 3650
# the real validity windows: CA to 2032-04-01, leaf to 2035
ca window_ca "$TEST_CA_DN" -not_before 20220404034211Z \
    -not_after 20320401034211Z

TEST_CA=(-CA ca.pem -CAkey ca.key.pem)
v1 leaf_v1.der "/CN=$SERIAL" leaf "${TEST_CA[@]}" -days 3650 -sha256
"$OPENSSL" req -new -x509 -subj "/CN=$SERIAL" -key leaf.key.pem \
    "${TEST_CA[@]}" -days 3650 -sha256 \
    -addext "subjectAltName=IP:192.0.2.15" -outform DER \
    -out "$OUT/leaf_v3.der" 2>&1 | { grep -v '^Warning' || true; }

# non-RSA leaf key (ECDSA P-256), still signed by the test CA
"$OPENSSL" genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 \
    -out ec.key.pem 2>/dev/null
"$OPENSSL" pkcs8 -topk8 -nocrypt -in ec.key.pem -outform DER \
    -out "$OUT/ec.pk8.der"
v1 ec_v1.der "/CN=$SERIAL" ec "${TEST_CA[@]}" -days 3650 -sha256

# a leaf that is its own issuer
v1 self_v1.der "/CN=$SERIAL" leaf -days 3650 -sha256
# a leaf from the V2-style device CA
v1 devca_leaf_v1.der "/CN=$SERIAL" leaf -CA devca.pem -CAkey devca.key.pem \
    -days 3650 -sha256
# forged leaves under the attacker CAs, served with the "other" key
v1 evil_leaf_v1.der "/CN=$SERIAL" other -CA evil_ca.pem \
    -CAkey evil_ca.key.pem -days 3650 -sha256
v1 evil_bbl_leaf_v1.der "/CN=$SERIAL" other -CA evil_bblca.pem \
    -CAkey evil_bblca.key.pem -days 3650 -sha256
# certificate signature algorithm other than sha256WithRSAEncryption
v1 sha384_v1.der "/CN=$SERIAL" leaf "${TEST_CA[@]}" -days 3650 -sha384
# subject without a CN, and with the serial CN twice
v1 no_cn_v1.der "/O=No CN Org" leaf "${TEST_CA[@]}" -days 3650 -sha256
v1 two_cn_v1.der "/CN=$SERIAL/CN=$SERIAL" leaf "${TEST_CA[@]}" -days 3650 \
    -sha256
# leaf with the real validity window under the window CA
v1 window_leaf_v1.der "/CN=$SERIAL" leaf -CA window_ca.pem \
    -CAkey window_ca.key.pem -not_before 20250615020127Z \
    -not_after 20350615020127Z -sha256

# keys of CAs are not needed by the tests
rm -f "$OUT/ca.pk8.der" "$OUT/evil_ca.pk8.der" "$OUT/devca.pk8.der" \
    "$OUT/window_ca.pk8.der"

for f in "$OUT"/*.der; do
  case "$f" in *.pk8.der) continue ;; esac
  printf '%-22s %s\n' "$(basename "$f")" "$("$OPENSSL" x509 -inform der \
      -in "$f" -noout -text | grep -E 'Version:|Signature Algorithm' \
      | head -2 | tr -s ' ' | tr '\n' ' ')"
done
