# list_tls test fixtures (MAN-536)

Test-only key material; nothing outside these tests trusts this CA.

The `list_http` unit tests and `tests/list_refresh.rs` serve TLS from
these files on 127.0.0.1 to prove that an `https://` list URL is fetched
with certificate and host-name checks.

- `ca.der`: the test CA, EC P-256, 100-year validity. Its private key was
  discarded after signing and is not committed.
- `localhost.der`, `localhost.key.der`: a leaf signed by that CA, SAN
  `IP:127.0.0.1, DNS:localhost`, EKU `serverAuth`; PKCS#8 DER key.
- `wrong-name.der`: the same key and CA, SAN `DNS:wrong.invalid`.

Made with:

```sh
openssl ecparam -name prime256v1 -genkey -noout -out ca.key.pem
openssl req -x509 -new -key ca.key.pem -sha256 -days 36500 -subj "/CN=manta list_http test CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" -out ca.pem
openssl ecparam -name prime256v1 -genkey -noout -out leaf.key.pem
openssl req -new -key leaf.key.pem -subj "/CN=localhost" -out leaf.csr
printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=IP:127.0.0.1,DNS:localhost\n' > leaf.ext
openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca.key.pem -CAcreateserial -sha256 -days 36500 -extfile leaf.ext -out leaf.pem
printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:wrong.invalid\n' > wrong.ext
openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca.key.pem -CAcreateserial -sha256 -days 36500 -extfile wrong.ext -out wrong.pem
openssl x509 -in ca.pem -outform DER -out ca.der
openssl x509 -in leaf.pem -outform DER -out localhost.der
openssl x509 -in wrong.pem -outform DER -out wrong-name.der
openssl pkcs8 -topk8 -nocrypt -in leaf.key.pem -outform DER -out localhost.key.der
rm ca.key.pem ca.pem ca.srl leaf.key.pem leaf.csr leaf.ext leaf.pem wrong.ext wrong.pem
```
