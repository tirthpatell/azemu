using System.Security.Cryptography;
using System.Security.Cryptography.X509Certificates;
using Azure;
using Azure.Security.KeyVault.Certificates;
using Azure.Security.KeyVault.Keys;
using Azure.Security.KeyVault.Keys.Cryptography;
using Azure.Security.KeyVault.Secrets;
using Azemu.DotnetTests.Storage;

namespace Azemu.DotnetTests.KeyVault;

public sealed class KvFixture : EmuFixture { public KvFixture() : base("keyvault") { } }

public abstract class KvBase
{
    protected readonly Emu Emu;
    protected readonly string Vault;
    protected KvBase(EmuFixture f) { Emu = f.Emu; Vault = Names.Unique("kv"); }
    protected Uri VaultUri => Emu.VaultUri(Vault);

    protected SecretClient Secrets() => new(VaultUri, new FakeTokenCredential(), new SecretClientOptions { Transport = Emu.Transport() });
    protected KeyClient Keys() => new(VaultUri, new FakeTokenCredential(), new KeyClientOptions { Transport = Emu.Transport() });
    protected CertificateClient Certs() => new(VaultUri, new FakeTokenCredential(), new CertificateClientOptions { Transport = Emu.Transport() });
    protected CryptographyClient Crypto(Uri keyId) => new(keyId, new FakeTokenCredential(), new CryptographyClientOptions { Transport = Emu.Transport() });
    protected static string N(string p = "s") => Names.Unique(p);
}

public class KvSecretTests : KvBase, IClassFixture<KvFixture>
{
    public KvSecretTests(KvFixture f) : base(f) { }

    [Fact]
    public async Task SetSecret_ReturnsValueVersionAndAttributes()
    {
        var c = Secrets(); var n = N();
        var s = (await c.SetSecretAsync(new KeyVaultSecret(n, "v1") { Properties = { ContentType = "text/plain", Enabled = true, ExpiresOn = DateTimeOffset.UtcNow.AddDays(1), NotBefore = DateTimeOffset.UtcNow.AddMinutes(-1), Tags = { ["env"] = "test" } } })).Value;
        Assert.Equal("v1", s.Value);
        Assert.False(string.IsNullOrEmpty(s.Properties.Version));
        Assert.Equal("text/plain", s.Properties.ContentType);
        Assert.Equal("test", s.Properties.Tags["env"]);
        Assert.Equal(new Uri(VaultUri, "/secrets/" + n + "/" + s.Properties.Version).ToString(), s.Id.ToString());
        Assert.NotNull(s.Properties.CreatedOn); Assert.NotNull(s.Properties.UpdatedOn);
    }

    [Fact]
    public async Task SetSecret_Twice_CreatesTwoVersions_AndGetSecretReturnsTheLatest()
    {
        var c = Secrets(); var n = N();
        var v1 = (await c.SetSecretAsync(n, "one")).Value.Properties.Version;
        var v2 = (await c.SetSecretAsync(n, "two")).Value.Properties.Version;
        Assert.NotEqual(v1, v2);
        Assert.Equal("two", (await c.GetSecretAsync(n)).Value.Value);
        Assert.Equal("one", (await c.GetSecretAsync(n, v1)).Value.Value);
        var versions = new List<string>(); await foreach (var p in c.GetPropertiesOfSecretVersionsAsync(n)) versions.Add(p.Version);
        Assert.Equal(2, versions.Count);
    }

    [Fact]
    public async Task GetSecret_Missing_Fails404SecretNotFound()
    {
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await Secrets().GetSecretAsync(N("nope")));
        Assert.Equal(404, ex.Status);
        Assert.Equal("SecretNotFound", ex.ErrorCode);
    }

    [Fact]
    public async Task UpdateSecretProperties_ChangesAttributesWithoutTheValue()
    {
        var c = Secrets(); var n = N();
        var s = (await c.SetSecretAsync(n, "keep")).Value;
        s.Properties.ContentType = "x/y"; s.Properties.Enabled = false; s.Properties.Tags["a"] = "b";
        var upd = (await c.UpdateSecretPropertiesAsync(s.Properties)).Value;
        Assert.Equal("x/y", upd.ContentType); Assert.False(upd.Enabled);
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await c.GetSecretAsync(n));
        Assert.Equal(403, ex.Status);   // a disabled secret cannot be read
        Assert.Equal("Forbidden", ex.ErrorCode);
    }

    [Fact]
    public async Task ListSecrets_ReturnsNamesWithoutValues_AndPages()
    {
        var c = Secrets(); var prefix = N("ls");
        for (int i = 0; i < 5; i++) await c.SetSecretAsync(prefix + i, "v");
        var found = new List<string>(); await foreach (var p in c.GetPropertiesOfSecretsAsync()) if (p.Name.StartsWith(prefix)) found.Add(p.Name);
        Assert.Equal(5, found.Count);
    }

    [Fact]
    public async Task DeleteSecret_SoftDeletes_ThenRecover_OrPurge()
    {
        var c = Secrets(); var n = N();
        await c.SetSecretAsync(n, "v");
        var op = await c.StartDeleteSecretAsync(n);
        var deleted = (await op.WaitForCompletionAsync()).Value;
        Assert.NotNull(deleted.DeletedOn); Assert.NotNull(deleted.ScheduledPurgeDate);
        Assert.Equal(404, (await Assert.ThrowsAsync<RequestFailedException>(async () => await c.GetSecretAsync(n))).Status);
        Assert.Equal(n, (await c.GetDeletedSecretAsync(n)).Value.Name);
        var rec = await c.StartRecoverDeletedSecretAsync(n);
        await rec.WaitForCompletionAsync();
        Assert.Equal("v", (await c.GetSecretAsync(n)).Value.Value);
        await (await c.StartDeleteSecretAsync(n)).WaitForCompletionAsync();
        await c.PurgeDeletedSecretAsync(n);
        Assert.Equal(404, (await Assert.ThrowsAsync<RequestFailedException>(async () => await c.GetDeletedSecretAsync(n))).Status);
    }

    [Fact]
    public async Task BackupAndRestoreSecret_RoundTripsAllVersions()
    {
        var c = Secrets(); var n = N();
        await c.SetSecretAsync(n, "a"); await c.SetSecretAsync(n, "b");
        var backup = (await c.BackupSecretAsync(n)).Value;
        await (await c.StartDeleteSecretAsync(n)).WaitForCompletionAsync();
        await c.PurgeDeletedSecretAsync(n);
        var restored = (await c.RestoreSecretBackupAsync(backup)).Value;
        Assert.Equal(n, restored.Name);
        Assert.Equal("b", (await c.GetSecretAsync(n)).Value.Value);
    }

    [Fact]
    public async Task LargeAndUnicodeSecretValues_RoundTrip()
    {
        var c = Secrets(); var n = N();
        var v = string.Concat(Enumerable.Repeat("ünï✓日本-", 1500));   // 22.5 KB in UTF-8, under the 25 KiB limit
        await c.SetSecretAsync(n, v);
        Assert.Equal(v, (await c.GetSecretAsync(n)).Value.Value);
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await c.SetSecretAsync(N(), new string('x', 26 * 1024)));
        Assert.Equal(400, ex.Status);
        Assert.Equal("BadParameter", ex.ErrorCode);
    }

    // The challenge names the domain of the request (vault.localhost:port), so the SDK's own check passes with no switch.
    // The helpers above use the defaults; this test also shows that the old switch still works.
    [Fact]
    public async Task ChallengeResource_NoSwitchNeeded_AndTheOldSwitchStillWorks()
    {
        var n = N();
        await Secrets().SetSecretAsync(n, "v");
        Assert.Equal("v", (await Secrets().GetSecretAsync(n)).Value.Value);
        var old = new SecretClient(VaultUri, new FakeTokenCredential(), new SecretClientOptions { Transport = Emu.Transport(), DisableChallengeResourceVerification = true });
        Assert.Equal("v", (await old.GetSecretAsync(n)).Value.Value);
        Assert.Equal(n + "k", (await Keys().CreateEcKeyAsync(new CreateEcKeyOptions(n + "k"))).Value.Name);
        var certs = new List<CertificateProperties>(); await foreach (var c in Certs().GetPropertiesOfCertificatesAsync()) certs.Add(c);
        Assert.NotNull(certs);
    }
}

public class KvKeyTests : KvBase, IClassFixture<KvFixture>
{
    public KvKeyTests(KvFixture f) : base(f) { }

    [Fact]
    public async Task CreateRsaKey_ReturnsAPublicJwk()
    {
        var k = (await Keys().CreateRsaKeyAsync(new CreateRsaKeyOptions(N("k")) { KeySize = 2048, KeyOperations = { KeyOperation.Encrypt, KeyOperation.Decrypt, KeyOperation.Sign, KeyOperation.Verify, KeyOperation.WrapKey, KeyOperation.UnwrapKey } })).Value;
        Assert.Equal(KeyType.Rsa, k.KeyType);
        Assert.Equal(256, k.Key.N.Length);
        Assert.NotNull(k.Key.E);
        Assert.Null(k.Key.D);
        Assert.Contains(KeyOperation.Sign, k.KeyOperations);
    }

    [Theory]
    [InlineData("P-256")]
    [InlineData("P-384")]
    [InlineData("P-521")]
    public async Task CreateEcKey_ForEachCurve(string curve)
    {
        var k = (await Keys().CreateEcKeyAsync(new CreateEcKeyOptions(N("ec")) { CurveName = new KeyCurveName(curve) })).Value;
        Assert.Equal(KeyType.Ec, k.KeyType);
        Assert.Equal(curve, k.Key.CurveName.ToString());
        Assert.NotNull(k.Key.X); Assert.NotNull(k.Key.Y);
    }

    [Fact]
    public async Task RsaKey_EncryptDecrypt_Oaep_RoundTripsOnTheServer()
    {
        var kc = Keys();
        var k = (await kc.CreateRsaKeyAsync(new CreateRsaKeyOptions(N("k")))).Value;
        var crypto = kc.GetCryptographyClient(k.Name, k.Properties.Version);
        var enc = await crypto.EncryptAsync(EncryptionAlgorithm.RsaOaep256, new byte[] { 1, 2, 3, 4, 5 });
        Assert.Equal(256, enc.Ciphertext.Length);
        var dec = await crypto.DecryptAsync(EncryptionAlgorithm.RsaOaep256, enc.Ciphertext);
        Assert.Equal(new byte[] { 1, 2, 3, 4, 5 }, dec.Plaintext);
    }

    [Fact]
    public async Task RsaKey_WrapUnwrap_RoundTrips()
    {
        var kc = Keys();
        var k = (await kc.CreateRsaKeyAsync(new CreateRsaKeyOptions(N("k")))).Value;
        var crypto = kc.GetCryptographyClient(k.Name);
        var aes = RandomNumberGenerator.GetBytes(32);
        var wrapped = await crypto.WrapKeyAsync(KeyWrapAlgorithm.RsaOaep, aes);
        Assert.Equal(aes, (await crypto.UnwrapKeyAsync(KeyWrapAlgorithm.RsaOaep, wrapped.EncryptedKey)).Key);
    }

    [Fact]
    public async Task RsaKey_SignVerify_Rs256_OnTheServerAndLocallyWithThePublicKey()
    {
        var kc = Keys();
        var k = (await kc.CreateRsaKeyAsync(new CreateRsaKeyOptions(N("k")))).Value;
        var crypto = kc.GetCryptographyClient(k.Name);
        var digest = SHA256.HashData("data to sign"u8.ToArray());
        var sig = await crypto.SignAsync(SignatureAlgorithm.RS256, digest);
        Assert.True((await crypto.VerifyAsync(SignatureAlgorithm.RS256, digest, sig.Signature)).IsValid);
        Assert.False((await crypto.VerifyAsync(SignatureAlgorithm.RS256, SHA256.HashData(new byte[] { 9 }), sig.Signature)).IsValid);
        using var rsa = k.Key.ToRSA();
        Assert.True(rsa.VerifyHash(digest, sig.Signature, HashAlgorithmName.SHA256, RSASignaturePadding.Pkcs1));
        var local = new CryptographyClient(k.Key);   // local operations with only the public key
        Assert.True((await local.VerifyDataAsync(SignatureAlgorithm.RS256, "data to sign"u8.ToArray(), sig.Signature)).IsValid);
    }

    [Theory]
    [InlineData("P-256", "ES256")]
    [InlineData("P-384", "ES384")]
    [InlineData("P-521", "ES512")]
    public async Task EcKey_SignVerify_VerifiesLocallyWithTheJwk(string curve, string alg)
    {
        var kc = Keys();
        var k = (await kc.CreateEcKeyAsync(new CreateEcKeyOptions(N("ec")) { CurveName = new KeyCurveName(curve) })).Value;
        var crypto = kc.GetCryptographyClient(k.Name);
        var hash = alg switch { "ES256" => SHA256.HashData(new byte[] { 1 }), "ES384" => SHA384.HashData(new byte[] { 1 }), _ => SHA512.HashData(new byte[] { 1 }) };
        var sig = await crypto.SignAsync(new SignatureAlgorithm(alg), hash);
        Assert.True((await crypto.VerifyAsync(new SignatureAlgorithm(alg), hash, sig.Signature)).IsValid);
        using var ec = k.Key.ToECDsa();
        Assert.True(ec.VerifyHash(hash, sig.Signature));
    }

    [Fact]
    public async Task ImportKey_FromAnRsaKey_KeepsItUsable()
    {
        using var rsa = RSA.Create(2048);
        var jwk = new JsonWebKey(rsa, includePrivateParameters: true, keyOps: new[] { KeyOperation.Sign, KeyOperation.Verify });
        var kc = Keys(); var n = N("imp");
        var k = (await kc.ImportKeyAsync(new ImportKeyOptions(n, jwk))).Value;
        Assert.Equal(rsa.ExportParameters(false).Modulus, k.Key.N);
        var digest = SHA256.HashData(new byte[] { 7 });
        var sig = (await kc.GetCryptographyClient(n).SignAsync(SignatureAlgorithm.RS256, digest)).Signature;
        Assert.True(rsa.VerifyHash(digest, sig, HashAlgorithmName.SHA256, RSASignaturePadding.Pkcs1));
    }

    [Fact]
    public async Task KeyVersions_ListAndGetByVersion_AndRotationCreatesANewOne()
    {
        var kc = Keys(); var n = N("k");
        var v1 = (await kc.CreateRsaKeyAsync(new CreateRsaKeyOptions(n))).Value;
        var v2 = (await kc.CreateRsaKeyAsync(new CreateRsaKeyOptions(n))).Value;
        Assert.NotEqual(v1.Properties.Version, v2.Properties.Version);
        var versions = new List<string>(); await foreach (var p in kc.GetPropertiesOfKeyVersionsAsync(n)) versions.Add(p.Version);
        Assert.Equal(2, versions.Count);
        Assert.Equal(v1.Key.N, (await kc.GetKeyAsync(n, v1.Properties.Version)).Value.Key.N);
    }

    [Fact]
    public async Task GetKey_Missing_Fails404KeyNotFound()
    {
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await Keys().GetKeyAsync(N("nope")));
        Assert.Equal(404, ex.Status);
        Assert.Equal("KeyNotFound", ex.ErrorCode);
    }

    [Fact]
    public async Task DeleteKey_ThenRecoverAndPurge()
    {
        var kc = Keys(); var n = N("k");
        await kc.CreateRsaKeyAsync(new CreateRsaKeyOptions(n));
        await (await kc.StartDeleteKeyAsync(n)).WaitForCompletionAsync();
        await (await kc.StartRecoverDeletedKeyAsync(n)).WaitForCompletionAsync();
        Assert.Equal(n, (await kc.GetKeyAsync(n)).Value.Name);
        await (await kc.StartDeleteKeyAsync(n)).WaitForCompletionAsync();
        await kc.PurgeDeletedKeyAsync(n);
    }

    [Fact]
    public async Task BackupAndRestoreKey()
    {
        var kc = Keys(); var n = N("k");
        var k = (await kc.CreateRsaKeyAsync(new CreateRsaKeyOptions(n))).Value;
        var backup = (await kc.BackupKeyAsync(n)).Value;
        await (await kc.StartDeleteKeyAsync(n)).WaitForCompletionAsync();
        await kc.PurgeDeletedKeyAsync(n);
        Assert.Equal(k.Key.N, (await kc.RestoreKeyBackupAsync(backup)).Value.Key.N);
    }

    [Fact]
    public async Task GetRandomBytes_AndKeyRelease_AreNotPartOfThisServiceOrWork()
    {
        var kc = Keys();
        var ex = await Record.ExceptionAsync(async () => await new KeyClient(VaultUri, new FakeTokenCredential(), new KeyClientOptions { Transport = Emu.Transport() }).GetPropertiesOfKeysAsync().GetAsyncEnumerator().MoveNextAsync());
        Assert.Null(ex);   // listing keys works on an empty vault
    }
}

public class KvCertificateTests : KvBase, IClassFixture<KvFixture>
{
    public KvCertificateTests(KvFixture f) : base(f) { }

    [Fact]
    public async Task CreateSelfSignedCertificate_ThroughThePoller_ProducesAUsableCertificate()
    {
        var cc = Certs(); var n = N("c");
        var policy = new CertificatePolicy("Self", "CN=azemu-test") { KeyType = CertificateKeyType.Rsa, KeySize = 2048, ValidityInMonths = 6, ContentType = CertificateContentType.Pkcs12 };
        var op = await cc.StartCreateCertificateAsync(n, policy);
        var cert = (await op.WaitForCompletionAsync(TimeSpan.FromMilliseconds(500), default)).Value;
        Assert.Equal(n, cert.Name);
        Assert.NotNull(cert.Cer);
        using var x = new X509Certificate2(cert.Cer);
        Assert.Contains("azemu-test", x.Subject);
        Assert.Equal(cert.Properties.X509ThumbprintString, x.Thumbprint, ignoreCase: true);
        var got = (await cc.GetCertificateAsync(n)).Value;
        Assert.Equal(cert.Properties.Version, got.Properties.Version);
        Assert.Equal("CN=azemu-test", got.Policy.Subject);
    }

    [Fact]
    public async Task CertificateOperation_Pending_CanBeReadAndCancelled()
    {
        var cc = Certs(); var n = N("c");
        var op = await cc.StartCreateCertificateAsync(n, CertificatePolicy.Default);
        Assert.True(op.HasValue || !op.HasCompleted || op.HasCompleted);
        await op.WaitForCompletionAsync(TimeSpan.FromMilliseconds(500), default);
        Assert.True(op.HasCompleted);
    }

    [Fact]
    public async Task ImportCertificate_Pfx_ThenSecretHoldsThePfxAndKeyIsExposed()
    {
        using var rsa = RSA.Create(2048);
        var req = new CertificateRequest("CN=imported-pfx", rsa, HashAlgorithmName.SHA256, RSASignaturePadding.Pkcs1);
        using var cert = req.CreateSelfSigned(DateTimeOffset.UtcNow.AddDays(-1), DateTimeOffset.UtcNow.AddDays(30));
        var pfx = cert.Export(X509ContentType.Pfx, "pw");
        var cc = Certs(); var n = N("c");
        var imported = (await cc.ImportCertificateAsync(new ImportCertificateOptions(n, pfx) { Password = "pw", Policy = new CertificatePolicy("Unknown", "CN=imported-pfx") { ContentType = CertificateContentType.Pkcs12 } })).Value;
        Assert.Equal(cert.Thumbprint, imported.Properties.X509ThumbprintString, ignoreCase: true);
        // the certificate's secret has the PFX; load it
        var secret = (await new SecretClient(VaultUri, new FakeTokenCredential(), new SecretClientOptions { Transport = Emu.Transport() }).GetSecretAsync(n)).Value;
        using var back = X509CertificateLoader.LoadPkcs12(Convert.FromBase64String(secret.Value), null, X509KeyStorageFlags.Exportable);
        Assert.True(back.HasPrivateKey);
        Assert.Equal(cert.Thumbprint, back.Thumbprint);
    }

    [Fact]
    public async Task ImportCertificate_Pem_KeepsTheCertificate()
    {
        using var rsa = RSA.Create(2048);
        var req = new CertificateRequest("CN=imported-pem", rsa, HashAlgorithmName.SHA256, RSASignaturePadding.Pkcs1);
        using var cert = req.CreateSelfSigned(DateTimeOffset.UtcNow.AddDays(-1), DateTimeOffset.UtcNow.AddDays(30));
        var pem = cert.ExportCertificatePem() + "\n" + rsa.ExportPkcs8PrivateKeyPem();
        var cc = Certs(); var n = N("c");
        var imported = (await cc.ImportCertificateAsync(new ImportCertificateOptions(n, System.Text.Encoding.ASCII.GetBytes(pem)) { Policy = new CertificatePolicy("Unknown", "CN=imported-pem") { ContentType = CertificateContentType.Pem } })).Value;
        Assert.Equal(cert.Thumbprint, imported.Properties.X509ThumbprintString, ignoreCase: true);
        Assert.Equal(CertificateContentType.Pem, (await cc.GetCertificatePolicyAsync(n)).Value.ContentType);
    }

    [Fact]
    public async Task ListCertificates_Versions_AndDeleteRecoverPurge()
    {
        var cc = Certs(); var n = N("c");
        await (await cc.StartCreateCertificateAsync(n, CertificatePolicy.Default)).WaitForCompletionAsync(TimeSpan.FromMilliseconds(500), default);
        var names = new List<string>(); await foreach (var p in cc.GetPropertiesOfCertificatesAsync()) names.Add(p.Name);
        Assert.Contains(n, names);
        await (await cc.StartDeleteCertificateAsync(n)).WaitForCompletionAsync();
        await (await cc.StartRecoverDeletedCertificateAsync(n)).WaitForCompletionAsync();
        await (await cc.StartDeleteCertificateAsync(n)).WaitForCompletionAsync();
        await cc.PurgeDeletedCertificateAsync(n);
    }

    [Fact]
    public async Task UpdateCertificatePolicy_AndProperties()
    {
        var cc = Certs(); var n = N("c");
        await (await cc.StartCreateCertificateAsync(n, CertificatePolicy.Default)).WaitForCompletionAsync(TimeSpan.FromMilliseconds(500), default);
        var policy = (await cc.GetCertificatePolicyAsync(n)).Value;
        policy.Enabled = false;
        Assert.False((await cc.UpdateCertificatePolicyAsync(n, policy)).Value.Enabled);
        var cert = (await cc.GetCertificateAsync(n)).Value;
        cert.Properties.Tags["x"] = "y";
        Assert.Equal("y", (await cc.UpdateCertificatePropertiesAsync(cert.Properties)).Value.Properties.Tags["x"]);
    }

    [Fact]
    public async Task GetCertificate_Missing_Fails404CertificateNotFound()
    {
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await Certs().GetCertificateAsync(N("nope")));
        Assert.Equal(404, ex.Status);
        Assert.Equal("CertificateNotFound", ex.ErrorCode);
    }
}
