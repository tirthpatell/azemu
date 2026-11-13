using Azure;
using Azure.Data.AppConfiguration;
using Azemu.DotnetTests.Storage;

namespace Azemu.DotnetTests.AppConfig;

public sealed class AcFixture : EmuFixture { public AcFixture() : base("appconfig") { } }

public class AppConfigTests : IClassFixture<AcFixture>
{
    readonly Emu _emu;
    readonly string _store = Names.Unique("ac");
    public AppConfigTests(AcFixture f) { _emu = f.Emu; }

    string Conn => $"Endpoint={_emu.AppConfigUri(_store)};Id=azemu-id;Secret={Convert.ToBase64String(new byte[] { 1, 2, 3, 4, 5, 6, 7, 8 })}";
    ConfigurationClient Client() => new(Conn, new ConfigurationClientOptions { Transport = _emu.Transport() });
    ConfigurationClient TokenClient() => new(_emu.AppConfigUri(_store), new FakeTokenCredential(), new ConfigurationClientOptions { Transport = _emu.Transport() });
    static string K(string p = "k") => Names.Unique(p);

    static async Task<List<T>> All<T>(AsyncPageable<T> p) where T : notnull { var l = new List<T>(); await foreach (var i in p) l.Add(i); return l; }

    [Fact]
    public async Task SetGetDelete_RoundTripsValueLabelContentTypeAndTags()
    {
        var c = Client(); var key = K();
        var s = new ConfigurationSetting(key, "v1", "prod") { ContentType = "text/plain" };
        s.Tags["team"] = "a";
        var set = (await c.SetConfigurationSettingAsync(s)).Value;
        Assert.Equal("v1", set.Value); Assert.False(string.IsNullOrEmpty(set.ETag.ToString())); Assert.NotNull(set.LastModified);
        var got = (await c.GetConfigurationSettingAsync(key, "prod")).Value;
        Assert.Equal("text/plain", got.ContentType); Assert.Equal("a", got.Tags["team"]);
        await c.DeleteConfigurationSettingAsync(key, "prod");
        Assert.Equal(404, (await Assert.ThrowsAsync<RequestFailedException>(async () => await c.GetConfigurationSettingAsync(key, "prod"))).Status);
    }

    [Fact]
    public async Task Get_WithAnotherLabel_IsNotFound_AndTheNullLabelIsADifferentSetting()
    {
        var c = Client(); var key = K();
        await c.SetConfigurationSettingAsync(key, "labelled", "L");
        await c.SetConfigurationSettingAsync(key, "plain");
        Assert.Equal("plain", (await c.GetConfigurationSettingAsync(key)).Value.Value);
        Assert.Equal("labelled", (await c.GetConfigurationSettingAsync(key, "L")).Value.Value);
        Assert.Equal(404, (await Assert.ThrowsAsync<RequestFailedException>(async () => await c.GetConfigurationSettingAsync(key, "other"))).Status);
    }

    [Fact]
    public async Task KeysWithSlashesColonsAndSpecialCharacters_RoundTrip()
    {
        var c = Client();
        foreach (var key in new[] { "app/db:conn/" + K(), "with space " + K(), "ünï/日本 " + K(), "a%b+c=d&e " + K() })
        {
            await c.SetConfigurationSettingAsync(key, "x", "lbl/with slash");
            Assert.Equal("x", (await c.GetConfigurationSettingAsync(key, "lbl/with slash")).Value.Value);
        }
    }

    [Fact]
    public async Task SetWithIfUnchanged_AndAStaleEtag_Fails412()
    {
        var c = Client(); var key = K();
        var first = (await c.SetConfigurationSettingAsync(key, "1")).Value;
        await c.SetConfigurationSettingAsync(key, "2");
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await c.SetConfigurationSettingAsync(first, onlyIfUnchanged: true));
        Assert.Equal(412, ex.Status);
        var current = (await c.GetConfigurationSettingAsync(key)).Value;
        current.Value = "3";
        Assert.Equal("3", (await c.SetConfigurationSettingAsync(current, onlyIfUnchanged: true)).Value.Value);
    }

    [Fact]
    public async Task AddConfigurationSetting_Twice_Fails412()
    {
        var c = Client(); var key = K();
        await c.AddConfigurationSettingAsync(key, "1");
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await c.AddConfigurationSettingAsync(key, "2"));
        Assert.Equal(412, ex.Status);
    }

    [Fact]
    public async Task ReadOnlyLock_BlocksChangesUntilUnlocked()
    {
        var c = Client(); var key = K();
        await c.SetConfigurationSettingAsync(key, "1");
        await c.SetReadOnlyAsync(key, null, true);
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await c.SetConfigurationSettingAsync(key, "2"));
        Assert.Equal(409, ex.Status);
        Assert.True((await c.GetConfigurationSettingAsync(key)).Value.IsReadOnly);
        await c.SetReadOnlyAsync(key, null, false);
        Assert.Equal("2", (await c.SetConfigurationSettingAsync(key, "2")).Value.Value);
    }

    [Fact]
    public async Task List_ByKeyFilterAndLabelFilter_WithWildcards()
    {
        var c = Client(); var p = K("lst");
        await c.SetConfigurationSettingAsync(p + "/a", "1", "x"); await c.SetConfigurationSettingAsync(p + "/b", "2", "y"); await c.SetConfigurationSettingAsync(p + "/c", "3", "x");
        var byKey = await All(c.GetConfigurationSettingsAsync(new SettingSelector { KeyFilter = p + "/*" }));
        Assert.Equal(3, byKey.Count);
        var byLabel = await All(c.GetConfigurationSettingsAsync(new SettingSelector { KeyFilter = p + "/*", LabelFilter = "x" }));
        Assert.Equal(new[] { p + "/a", p + "/c" }, byLabel.Select(s => s.Key).OrderBy(x => x).ToArray());
        var exact = await All(c.GetConfigurationSettingsAsync(new SettingSelector { KeyFilter = p + "/b" }));
        Assert.Equal("2", Assert.Single(exact).Value);
    }

    [Fact]
    public async Task List_WithFieldsSelection_AndByTags()
    {
        var c = Client(); var p = K("tg");
        var s = new ConfigurationSetting(p, "val", "L"); s.Tags["t1"] = "v1";
        await c.SetConfigurationSettingAsync(s);
        var sel = new SettingSelector { KeyFilter = p, Fields = SettingFields.Key | SettingFields.Label };
        var r = Assert.Single(await All(c.GetConfigurationSettingsAsync(sel)));
        Assert.Equal(p, r.Key); Assert.Null(r.Value);
        var sel2 = new SettingSelector { KeyFilter = p }; sel2.TagsFilter.Add("t1=v1");
        Assert.Single(await All(c.GetConfigurationSettingsAsync(sel2)));
        var sel3 = new SettingSelector { KeyFilter = p }; sel3.TagsFilter.Add("t1=nope");
        Assert.Empty(await All(c.GetConfigurationSettingsAsync(sel3)));
    }

    [Fact]
    public async Task List_ManySettings_FollowsNextLinkPages()
    {
        var c = Client(); var p = K("pg");
        for (int i = 0; i < 25; i++) await c.SetConfigurationSettingAsync($"{p}/{i:D2}", "v");
        var pages = 0; var n = 0;
        await foreach (var page in c.GetConfigurationSettingsAsync(new SettingSelector { KeyFilter = p + "/*" }).AsPages()) { pages++; n += page.Values.Count; }
        Assert.Equal(25, n);
        Assert.True(pages >= 1);
    }

    [Fact]
    public async Task Revisions_KeepOlderValues_NewestFirst()
    {
        var c = Client(); var key = K();
        await c.SetConfigurationSettingAsync(key, "1"); await c.SetConfigurationSettingAsync(key, "2"); await c.SetConfigurationSettingAsync(key, "3");
        var revs = await All(c.GetRevisionsAsync(new SettingSelector { KeyFilter = key }));
        Assert.Equal(new[] { "3", "2", "1" }, revs.Select(r => r.Value).ToArray());
    }

    [Fact]
    public async Task FeatureFlagAndSecretReferenceSettings_RoundTripTheirTypes()
    {
        var c = Client(); var id = K("ff");
        var ff = new FeatureFlagConfigurationSetting(id, true) { Description = "d", DisplayName = "name" };
        ff.ClientFilters.Add(new FeatureFlagFilter("Microsoft.Percentage", new Dictionary<string, object> { ["Value"] = 50 }));
        await c.SetConfigurationSettingAsync(ff);
        var got = (FeatureFlagConfigurationSetting)(await c.GetConfigurationSettingAsync(ff.Key)).Value;
        Assert.True(got.IsEnabled); Assert.Equal("name", got.DisplayName);
        Assert.Equal("Microsoft.Percentage", Assert.Single(got.ClientFilters).Name);
        var sr = new SecretReferenceConfigurationSetting(K("sr"), new Uri("https://vault.example.net/secrets/x"));
        await c.SetConfigurationSettingAsync(sr);
        var back = (SecretReferenceConfigurationSetting)(await c.GetConfigurationSettingAsync(sr.Key)).Value;
        Assert.Equal(new Uri("https://vault.example.net/secrets/x"), back.SecretId);
    }

    [Fact]
    public async Task Snapshots_CreateWithPoller_ListSettingsAndArchive()
    {
        var c = Client(); var p = K("sn");
        await c.SetConfigurationSettingAsync(p + "/a", "1"); await c.SetConfigurationSettingAsync(p + "/b", "2");
        var snap = new ConfigurationSnapshot(new[] { new ConfigurationSettingsFilter(p + "/*") });
        var op = await c.CreateSnapshotAsync(WaitUntil.Completed, p + "-snap", snap);
        Assert.Equal(ConfigurationSnapshotStatus.Ready, op.Value.Status);
        await c.SetConfigurationSettingAsync(p + "/c", "3");
        var inSnap = await All(c.GetConfigurationSettingsForSnapshotAsync(p + "-snap"));
        Assert.Equal(2, inSnap.Count);
        var archived = (await c.ArchiveSnapshotAsync(p + "-snap")).Value;
        Assert.Equal(ConfigurationSnapshotStatus.Archived, archived.Status);
        Assert.Equal(404, (await Assert.ThrowsAsync<RequestFailedException>(async () => await c.GetSnapshotAsync(p + "-missing"))).Status);
    }

    [Fact]
    public async Task TokenCredential_Works_AsWellAsTheConnectionStringSecret()
    {
        var key = K();
        await TokenClient().SetConfigurationSettingAsync(key, "via-token");
        Assert.Equal("via-token", (await Client().GetConfigurationSettingAsync(key)).Value.Value);
    }

    [Fact]
    public async Task ConditionalGet_WithTheCurrentEtag_Returns304()
    {
        var c = Client(); var key = K();
        var s = (await c.SetConfigurationSettingAsync(key, "1")).Value;
        var r = await c.GetConfigurationSettingAsync(s, onlyIfChanged: true);
        Assert.Equal(304, r.GetRawResponse().Status);
    }

    [Fact]
    public async Task Delete_Missing_IsA204_NotAnError()
    {
        var c = Client();
        var r = await c.DeleteConfigurationSettingAsync(K("never"));
        Assert.Equal(204, r.Status);
    }
}
