# Test TLS material (feature 09: rift#977 mutualAuth, rift#974 `--upstream-ca-file`)

These are throwaway certificates for tests only. Never trust them anywhere else.

All three leaves are signed by the existing test CA, `../intercept/ca-cert.pem` /
`../intercept/ca-key.pem` (CN=rift-conformance-test-intercept-ca, `CA:TRUE`, expires 2046). That is
the CA the `mutual-auth-ca` imposter's `ca` embeds, and the one the harness hands to
`--upstream-ca-file`.

| File | What it is |
|---|---|
| `client-cert.pem` / `client-key.pem` | client leaf signed by the test CA (EKU `clientAuth`) |
| `other-client-cert.pem` / `other-client-key.pem` | unrelated self-signed client cert (EKU `clientAuth`) |
| `server-cert.pem` / `server-key.pem` | server leaf for `localhost` / `127.0.0.1`, signed by the test CA (EKU `serverAuth`) |
| `mutual-auth-ca.json` | imposter: `mutualAuth` + `rejectUnauthorized` + `ca` = the test CA |
| `mutual-auth-any.json` | imposter: `mutualAuth` only |
| `upstream.json` | HTTPS imposter serving `server-cert.pem` |

Keys are EC P-256 in PKCS#8 (`BEGIN PRIVATE KEY`), which the harness loads with the JDK's
`PKCS8EncodedKeySpec`. Leaves are valid for 7300 days.

Regenerate from this directory:

```sh
CA=../intercept
T=$(mktemp -d)
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out client-key.pem
openssl req -new -key client-key.pem -subj "/CN=rift-conformance-test-client" -out $T/client.csr
printf "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\n" > $T/client.ext
openssl x509 -req -in $T/client.csr -CA $CA/ca-cert.pem -CAkey $CA/ca-key.pem -CAcreateserial \
  -CAserial $T/ca.srl -days 7300 -sha256 -extfile $T/client.ext -out client-cert.pem

openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out other-client-key.pem
openssl req -new -x509 -key other-client-key.pem -subj "/CN=rift-conformance-test-unrelated-client" \
  -days 7300 -sha256 -addext "extendedKeyUsage=clientAuth" -out other-client-cert.pem

openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out server-key.pem
openssl req -new -key server-key.pem -subj "/CN=localhost" -out $T/server.csr
printf "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n" > $T/server.ext
openssl x509 -req -in $T/server.csr -CA $CA/ca-cert.pem -CAkey $CA/ca-key.pem \
  -CAserial $T/ca.srl -days 7300 -sha256 -extfile $T/server.ext -out server-cert.pem
```

### Feature 20 (TLS termination, #197)

| File | What it is |
|---|---|
| `server-leaf.pem` | RSA server leaf for `localhost` / `127.0.0.1`, signed by the test CA (CN=rift-conformance-test-server-leaf) |
| `server-leaf.key` / `server-leaf-rsa.key` | its one RSA key, as PKCS#8 (`BEGIN PRIVATE KEY`) and PKCS#1 (`BEGIN RSA PRIVATE KEY`) |
| `intermediate.pem` | intermediate CA signed by the test CA |
| `chain-leaf.pem` / `chain-leaf.key` | server leaf for `localhost` signed by the intermediate; served with `intermediate.pem` as a leaf-first chain |
| `client-serverauth-only-cert.pem` / `-key.pem` | client cert signed by the test CA whose EKU is `serverAuth` only |

Regenerate these from this directory (the intermediate's key is thrown away):

```sh
CA=../intercept
T=$(mktemp -d)
printf "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n" > $T/leaf.ext
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out server-leaf.key
openssl rsa -in server-leaf.key -traditional -out server-leaf-rsa.key
openssl req -new -key server-leaf.key -subj "/CN=rift-conformance-test-server-leaf" -out $T/leaf.csr
openssl x509 -req -in $T/leaf.csr -CA $CA/ca-cert.pem -CAkey $CA/ca-key.pem -CAcreateserial \
  -CAserial $T/ca.srl -days 7300 -sha256 -extfile $T/leaf.ext -out server-leaf.pem
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out $T/int.key
openssl req -new -key $T/int.key -subj "/CN=rift-conformance-test-intermediate" -out $T/int.csr
printf "basicConstraints=critical,CA:TRUE,pathlen:0\nkeyUsage=critical,keyCertSign,cRLSign\n" > $T/int.ext
openssl x509 -req -in $T/int.csr -CA $CA/ca-cert.pem -CAkey $CA/ca-key.pem \
  -CAserial $T/ca.srl -days 7300 -sha256 -extfile $T/int.ext -out intermediate.pem
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out chain-leaf.key
openssl req -new -key chain-leaf.key -subj "/CN=rift-conformance-test-chain-leaf" -out $T/cl.csr
openssl x509 -req -in $T/cl.csr -CA intermediate.pem -CAkey $T/int.key -CAcreateserial \
  -CAserial $T/int.srl -days 7300 -sha256 -extfile $T/leaf.ext -out chain-leaf.pem
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out client-serverauth-only-key.pem
openssl req -new -key client-serverauth-only-key.pem -subj "/CN=rift-conformance-test-serverauth-only" -out $T/sa.csr
printf "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n" > $T/sa.ext
openssl x509 -req -in $T/sa.csr -CA $CA/ca-cert.pem -CAkey $CA/ca-key.pem \
  -CAserial $T/ca.srl -days 7300 -sha256 -extfile $T/sa.ext -out client-serverauth-only-cert.pem
rm -rf $T
```

The imposter JSONs embed PEM text. After regenerating, rewrite the `ca` field of
`mutual-auth-ca.json` and the `cert` / `key` fields of `upstream.json`.
