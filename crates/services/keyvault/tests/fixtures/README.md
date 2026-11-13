# Key Vault test fixtures

Every key, certificate and PKCS#12 file in this directory was **generated for the tests** of this repository (the certificate
subject is `CN=fixture-cn`). They protect nothing,
they are public on purpose, and no real system trusts them. **Never use them as keys.** None of it was found in Microsoft's SDK tests or recordings; nothing here is known to come from another project.

| File | What it is |
|---|---|
| `rsa2048.json` | an RSA 2048 key as a JSON Web Key (with its private parts), and signatures and ciphertexts computed with it |
| `bundle.pem` | a private key and a self-signed certificate (`CN=fixture-cn`) in one PEM file |
| `fixture.cer.b64` | the same certificate (same SHA-256 fingerprint), DER, base64 |
| `plain.pfx.b64`, `password.pfx.b64` | PKCS#12 files for the certificate `CN=fixture-cn` (the first opens with an empty password, the second with the password that `certificates.rs` uses) |
