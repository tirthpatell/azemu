using Azure.Core;
using Azure.Identity;
using Azure.Security.KeyVault.Secrets;
using Azure.Storage.Blobs;
using Azure.Storage;
using System.Text.Json;

namespace Azemu.DotnetTests.IdentityTests;

public sealed class IdFixture : EmuFixture { public IdFixture() : base("identity") { } }

/// <summary>Credential classes of Azure.Identity against the emulator's token endpoints (docs/services/identity.md).</summary>
public class IdentityTests : IClassFixture<IdFixture>
{
    readonly Emu _emu;
    public IdentityTests(IdFixture f) { _emu = f.Emu; }

    sealed class JwtInfo
    {
        public JsonElement Header, Payload;
        public string Claim(string n) => Payload.GetProperty(n).GetString()!;
        public string Aud => Claim("aud");
        public string Kid => Header.GetProperty("kid").GetString()!;
        public string Alg => Header.GetProperty("alg").GetString()!;
    }

    static JwtInfo Jwt(string token)
    {
        static byte[] B64(string s) { s = s.Replace('-', '+').Replace('_', '/'); return Convert.FromBase64String(s.PadRight(s.Length + (4 - s.Length % 4) % 4, '=')); }
        var parts = token.Split('.');
        Assert.Equal(3, parts.Length);
        return new JwtInfo { Header = JsonDocument.Parse(B64(parts[0])).RootElement.Clone(), Payload = JsonDocument.Parse(B64(parts[1])).RootElement.Clone() };
    }

    Uri Authority => new($"https://login.localhost:{_emu.Port}/");
    string MsiEndpoint => $"http://localhost:{_emu.Port}/_azemu/identity/msi/token";

    /// <summary>Runs the code with the App Service style variables set; they are restored after.</summary>
    static async Task WithEnv(Dictionary<string, string?> vars, Func<Task> body)
    {
        var old = vars.ToDictionary(k => k.Key, k => Environment.GetEnvironmentVariable(k.Key));
        try { foreach (var kv in vars) Environment.SetEnvironmentVariable(kv.Key, kv.Value); await body(); }
        finally { foreach (var kv in old) Environment.SetEnvironmentVariable(kv.Key, kv.Value); }
    }

    [Fact]
    public async Task ManagedIdentityCredential_ThroughIdentityEndpoint_ReturnsAJwtForTheResource()
    {
        await WithEnv(new() { ["IDENTITY_ENDPOINT"] = MsiEndpoint, ["IDENTITY_HEADER"] = "azemu" }, async () =>
        {
            var cred = new ManagedIdentityCredential(new ManagedIdentityCredentialOptions(ManagedIdentityId.SystemAssigned) { Transport = _emu.Transport() });
            var t = await cred.GetTokenAsync(new TokenRequestContext(new[] { "https://storage.azure.com/.default" }));
            var jwt = Jwt(t.Token);
            Assert.Equal("https://storage.azure.com", jwt.Aud);
            Assert.True(t.ExpiresOn > DateTimeOffset.UtcNow.AddMinutes(30));
        });
    }

    [Fact]
    public async Task ManagedIdentityCredential_UserAssigned_ClientIdIsHonoured()
    {
        var clientId = Guid.NewGuid().ToString();
        await WithEnv(new() { ["IDENTITY_ENDPOINT"] = MsiEndpoint, ["IDENTITY_HEADER"] = "azemu" }, async () =>
        {
            var cred = new ManagedIdentityCredential(new ManagedIdentityCredentialOptions(ManagedIdentityId.FromUserAssignedClientId(clientId)) { Transport = _emu.Transport() });
            var t = await cred.GetTokenAsync(new TokenRequestContext(new[] { "https://vault.azure.net/.default" }));
            var jwt = Jwt(t.Token);
            Assert.Equal(clientId, jwt.Claim("appid"));
        });
    }

    [Fact]
    public async Task DefaultAzureCredential_UsesTheManagedIdentityEndpoint_WithNoOtherCredentialsAround()
    {
        await WithEnv(new() { ["IDENTITY_ENDPOINT"] = MsiEndpoint, ["IDENTITY_HEADER"] = "azemu", ["AZURE_CLIENT_ID"] = null, ["AZURE_TENANT_ID"] = null, ["AZURE_CLIENT_SECRET"] = null }, async () =>
        {
            var cred = new DefaultAzureCredential(new DefaultAzureCredentialOptions { Transport = _emu.Transport(), ExcludeAzureCliCredential = true, ExcludeAzurePowerShellCredential = true, ExcludeAzureDeveloperCliCredential = true, ExcludeVisualStudioCredential = true, ExcludeInteractiveBrowserCredential = true, ExcludeWorkloadIdentityCredential = true, ExcludeEnvironmentCredential = true });
            var t = await cred.GetTokenAsync(new TokenRequestContext(new[] { "https://management.azure.com/.default" }));
            Assert.Equal("https://management.azure.com", Jwt(t.Token).Aud);
        });
    }

    [Fact]
    public async Task ClientSecretCredential_WithTheEmulatorAuthority_ReturnsATokenWithTheTenantAndClient()
    {
        var tenant = Guid.NewGuid().ToString(); var client = Guid.NewGuid().ToString();
        var cred = new ClientSecretCredential(tenant, client, "any-secret", new ClientSecretCredentialOptions { AuthorityHost = Authority, DisableInstanceDiscovery = true, Transport = _emu.Transport() });
        var t = await cred.GetTokenAsync(new TokenRequestContext(new[] { "https://storage.azure.com/.default" }));
        var jwt = Jwt(t.Token);
        Assert.Equal(tenant, jwt.Claim("tid"));
        Assert.Equal(client, jwt.Claim("appid"));
        Assert.Equal("https://storage.azure.com", jwt.Aud);
    }

    [Fact]
    public async Task ClientSecretCredential_TokensAreCachedUntilNearExpiry()
    {
        var cred = new ClientSecretCredential(Guid.NewGuid().ToString(), Guid.NewGuid().ToString(), "s", new ClientSecretCredentialOptions { AuthorityHost = Authority, DisableInstanceDiscovery = true, Transport = _emu.Transport() });
        var ctx = new TokenRequestContext(new[] { "https://vault.azure.net/.default" });
        var a = await cred.GetTokenAsync(ctx); var b = await cred.GetTokenAsync(ctx);
        Assert.Equal(a.Token, b.Token);
    }

    [Fact]
    public async Task ClientSecretCredential_JwtIsSignedWithAKeyFromTheJwksEndpoint()
    {
        var cred = new ClientSecretCredential("contoso.example", Guid.NewGuid().ToString(), "s", new ClientSecretCredentialOptions { AuthorityHost = Authority, DisableInstanceDiscovery = true, Transport = _emu.Transport() });
        var t = await cred.GetTokenAsync(new TokenRequestContext(new[] { "https://storage.azure.com/.default" }));
        var jwt = Jwt(t.Token);
        using var http = _emu.NewHttpClient();
        var keys = await http.GetStringAsync($"https://login.localhost:{_emu.Port}/{jwt.Claim("tid")}/discovery/v2.0/keys");
        Assert.Contains(jwt.Kid, keys);
        Assert.Equal("RS256", jwt.Alg);
    }

    [Fact]
    public async Task ClientSecretCredential_AnUnreachableAuthority_FailsWithAuthenticationFailed()
    {
        var cred = new ClientSecretCredential(Guid.NewGuid().ToString(), Guid.NewGuid().ToString(), "s", new ClientSecretCredentialOptions { AuthorityHost = new Uri("https://login.invalid.localhost:1/"), DisableInstanceDiscovery = true, Transport = _emu.Transport(), Retry = { MaxRetries = 0 } });
        await Assert.ThrowsAnyAsync<Exception>(async () => await cred.GetTokenAsync(new TokenRequestContext(new[] { "https://storage.azure.com/.default" })));
    }

    [Fact]
    public async Task TokenFromTheEmulator_IsAcceptedAsABearerTokenByKeyVaultAndBlob()
    {
        await WithEnv(new() { ["IDENTITY_ENDPOINT"] = MsiEndpoint, ["IDENTITY_HEADER"] = "azemu" }, async () =>
        {
            var cred = new ManagedIdentityCredential(new ManagedIdentityCredentialOptions(ManagedIdentityId.SystemAssigned) { Transport = _emu.Transport() });
            var vault = Names.Unique("kv");
            var sc = new SecretClient(_emu.VaultUri(vault), cred, new SecretClientOptions { Transport = _emu.Transport() });
            await sc.SetSecretAsync("s", "via-msi");
            Assert.Equal("via-msi", (await sc.GetSecretAsync("s")).Value.Value);
            var acct = Names.Unique("sa");
            var bs = new BlobServiceClient(new Uri($"https://{acct}.blob.core.localhost:{_emu.Port}"), cred, new BlobClientOptions { Transport = _emu.Transport() });
            var c = bs.GetBlobContainerClient(Names.Unique("c"));
            await c.CreateAsync();
            await c.GetBlobClient("b").UploadAsync(BinaryData.FromString("oauth"));
            Assert.Equal("oauth", (await c.GetBlobClient("b").DownloadContentAsync()).Value.Content.ToString());
        });
    }

    [Fact]
    public async Task ChainedTokenCredential_SkipsAnUnavailableCredential_AndUsesTheWorkingOne()
    {
        // a chain only moves on from a credential that says "unavailable" (no settings), not from one that failed
        await WithEnv(new() { ["AZURE_TENANT_ID"] = null, ["AZURE_CLIENT_ID"] = null, ["AZURE_CLIENT_SECRET"] = null, ["AZURE_USERNAME"] = null, ["AZURE_PASSWORD"] = null, ["AZURE_CLIENT_CERTIFICATE_PATH"] = null }, async () =>
        {
            var unavailable = new EnvironmentCredential();
            var good = new ClientSecretCredential(Guid.NewGuid().ToString(), Guid.NewGuid().ToString(), "s", new ClientSecretCredentialOptions { AuthorityHost = Authority, DisableInstanceDiscovery = true, Transport = _emu.Transport() });
            var t = await new ChainedTokenCredential(unavailable, good).GetTokenAsync(new TokenRequestContext(new[] { "https://storage.azure.com/.default" }));
            Assert.False(string.IsNullOrEmpty(t.Token));
            var bad = new ClientSecretCredential(Guid.NewGuid().ToString(), Guid.NewGuid().ToString(), "s", new ClientSecretCredentialOptions { AuthorityHost = new Uri("https://login.invalid.localhost:1/"), DisableInstanceDiscovery = true, Transport = _emu.Transport(), Retry = { MaxRetries = 0 } });
            await Assert.ThrowsAsync<AuthenticationFailedException>(async () => await new ChainedTokenCredential(bad, good).GetTokenAsync(new TokenRequestContext(new[] { "https://storage.azure.com/.default" })));
        });
    }
}
