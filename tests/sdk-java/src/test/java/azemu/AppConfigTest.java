package azemu;

import com.azure.core.http.rest.PagedResponse;
import com.azure.core.exception.HttpResponseException;
import com.azure.core.exception.ResourceModifiedException;
import com.azure.core.exception.ResourceNotFoundException;
import com.azure.core.http.rest.Response;
import com.azure.core.util.Context;
import com.azure.data.appconfiguration.ConfigurationClient;
import com.azure.data.appconfiguration.ConfigurationClientBuilder;
import com.azure.data.appconfiguration.models.*;
import org.junit.jupiter.api.*;

import java.time.Duration;
import java.util.*;
import java.util.stream.Collectors;

import static org.junit.jupiter.api.Assertions.*;

/** App Configuration: settings, labels, tags, ETag conditions, locks, revisions, feature flags, secret references, snapshots, paging. */
@EmuTest
class AppConfigTest {
    static Emu emu;
    static ConfigurationClient client;
    static String store;

    @BeforeAll static void setUp() {
        emu = Emu.current();
        store = Emu.name("acjava");
        client = new ConfigurationClientBuilder()
            .connectionString("Endpoint=" + emu.appConfigUrl(store) + ";Id=azemu;Secret=" + Base64.getEncoder().encodeToString("secret".getBytes()))
            .buildClient();
    }

    static String key() { return Emu.dashed("k"); }

    @Test void setAndGet_roundTripsValueLabelContentTypeAndTags() {
        String k = key();
        ConfigurationSetting set = client.setConfigurationSetting(new ConfigurationSetting().setKey(k).setLabel("prod").setValue("v1")
            .setContentType("text/plain").setTags(Map.of("team", "a")));
        assertEquals(k, set.getKey());
        assertNotNull(set.getETag());
        assertNotNull(set.getLastModified());
        ConfigurationSetting got = client.getConfigurationSetting(k, "prod");
        assertEquals("v1", got.getValue());
        assertEquals("prod", got.getLabel());
        assertEquals("text/plain", got.getContentType());
        assertEquals("a", got.getTags().get("team"));
        assertEquals(set.getETag(), got.getETag());
    }

    @Test void nullLabelAndALabeledSetting_areDifferentSettings() {
        String k = key();
        client.setConfigurationSetting(k, null, "no label");
        client.setConfigurationSetting(k, "dev", "dev label");
        assertEquals("no label", client.getConfigurationSetting(k, null).getValue());
        assertEquals("dev label", client.getConfigurationSetting(k, "dev").getValue());
    }

    @Test void getMissing_is404() {
        HttpResponseException e = assertThrows(HttpResponseException.class, () -> client.getConfigurationSetting(key(), null));
        assertEquals(404, e.getResponse().getStatusCode());
    }

    @Test void addExisting_isRefusedWith412() {
        String k = key();
        client.addConfigurationSetting(k, null, "v");
        HttpResponseException e = assertThrows(HttpResponseException.class, () -> client.addConfigurationSetting(k, null, "again"));
        assertEquals(412, e.getResponse().getStatusCode());
    }

    @Test void setWithIfUnchanged_failsWhenTheEtagIsStale() {
        String k = key();
        ConfigurationSetting v1 = client.setConfigurationSetting(k, null, "one");
        client.setConfigurationSetting(k, null, "two");
        HttpResponseException e = assertThrows(HttpResponseException.class,
            () -> client.setConfigurationSettingWithResponse(v1, true, Context.NONE));
        assertEquals(412, e.getResponse().getStatusCode());
        // without the condition it works
        assertEquals("one", client.setConfigurationSettingWithResponse(v1, false, Context.NONE).getValue().getValue());
    }

    @Test void getWithIfChanged_returns304WhenNothingChanged() {
        String k = key();
        ConfigurationSetting s = client.setConfigurationSetting(k, null, "v");
        Response<ConfigurationSetting> r = client.getConfigurationSettingWithResponse(s, null, true, Context.NONE);
        assertEquals(304, r.getStatusCode());
    }

    @Test void deleteReturnsTheSetting_andDeleteWithStaleEtagFails() {
        String k = key();
        ConfigurationSetting v1 = client.setConfigurationSetting(k, null, "one");
        client.setConfigurationSetting(k, null, "two");
        HttpResponseException e = assertThrows(HttpResponseException.class, () -> client.deleteConfigurationSettingWithResponse(v1, true, Context.NONE));
        assertEquals(412, e.getResponse().getStatusCode());
        ConfigurationSetting deleted = client.deleteConfigurationSetting(k, null);
        assertEquals("two", deleted.getValue());
        assertEquals(404, assertThrows(HttpResponseException.class, () -> client.getConfigurationSetting(k, null)).getResponse().getStatusCode());
    }

    @Test void readOnly_blocksWritesUntilUnlocked() {
        String k = key();
        client.setConfigurationSetting(k, null, "v");
        assertTrue(client.setReadOnly(k, null, true).isReadOnly());
        HttpResponseException e = assertThrows(HttpResponseException.class, () -> client.setConfigurationSetting(k, null, "w"));
        assertEquals(409, e.getResponse().getStatusCode());
        assertFalse(client.setReadOnly(k, null, false).isReadOnly());
        assertEquals("w", client.setConfigurationSetting(k, null, "w").getValue());
    }

    @Test void listWithKeyAndLabelFilters_andWildcards() {
        String p = key();
        client.setConfigurationSetting(p + "-a", "x", "1");
        client.setConfigurationSetting(p + "-b", "x", "2");
        client.setConfigurationSetting(p + "-b", "y", "3");
        client.setConfigurationSetting(p + "-c", null, "4");
        assertEquals(4, client.listConfigurationSettings(new SettingSelector().setKeyFilter(p + "*")).stream().count());
        assertEquals(2, client.listConfigurationSettings(new SettingSelector().setKeyFilter(p + "-*").setLabelFilter("x")).stream().count());
        assertEquals(1, client.listConfigurationSettings(new SettingSelector().setKeyFilter(p + "-b").setLabelFilter("y")).stream().count());
        assertEquals(1, client.listConfigurationSettings(new SettingSelector().setKeyFilter(p + "*").setLabelFilter("\0")).stream().count(), "null label");
        List<String> keysOnly = client.listConfigurationSettings(new SettingSelector().setKeyFilter(p + "*").setFields(SettingFields.KEY))
            .stream().map(ConfigurationSetting::getKey).collect(Collectors.toList());
        assertEquals(4, keysOnly.size());
    }

    @Test void listWithTagFilter() {
        String p = key();
        client.setConfigurationSetting(new ConfigurationSetting().setKey(p + "-1").setValue("a").setTags(Map.of("env", "prod")));
        client.setConfigurationSetting(new ConfigurationSetting().setKey(p + "-2").setValue("b").setTags(Map.of("env", "dev")));
        assertEquals(1, client.listConfigurationSettings(new SettingSelector().setKeyFilter(p + "*").setTagsFilter(List.of("env=prod"))).stream().count());
    }

    @Test void paging_returnsEverySettingAcrossPages() {
        String p = key();
        for (int i = 0; i < 130; i++) client.setConfigurationSetting(p + "-" + String.format("%03d", i), null, "v" + i);
        int pages = 0, items = 0;
        for (PagedResponse<ConfigurationSetting> page : client.listConfigurationSettings(new SettingSelector().setKeyFilter(p + "*")).iterableByPage()) {
            pages++;
            items += page.getValue().size();
        }
        assertEquals(130, items);
        assertTrue(pages >= 2, "more than 100 items need a second page, got " + pages);
    }

    @Test void revisions_listEveryChangeOfAKey() {
        String k = key();
        client.setConfigurationSetting(k, null, "v1");
        client.setConfigurationSetting(k, null, "v2");
        client.setConfigurationSetting(k, null, "v3");
        List<String> values = client.listRevisions(new SettingSelector().setKeyFilter(k)).stream().map(ConfigurationSetting::getValue).collect(Collectors.toList());
        assertEquals(3, values.size());
        assertEquals(Set.of("v1", "v2", "v3"), new HashSet<>(values));
    }

    @Test void labels_areListed() {
        String k = key();
        client.setConfigurationSetting(k, "lbl-" + k, "v");
        assertTrue(client.listLabels().stream().anyMatch(l -> ("lbl-" + k).equals(l.getName())));
    }

    @Test void featureFlag_roundTripsAsAFeatureFlagSetting() {
        String id = key();
        FeatureFlagConfigurationSetting ff = new FeatureFlagConfigurationSetting(id, true).setDescription("beta").setDisplayName("Beta");
        client.setConfigurationSetting(ff);
        ConfigurationSetting got = client.getConfigurationSetting(ff.getKey(), null);
        assertInstanceOf(FeatureFlagConfigurationSetting.class, got);
        FeatureFlagConfigurationSetting f = (FeatureFlagConfigurationSetting) got;
        assertTrue(f.isEnabled());
        assertEquals("beta", f.getDescription());
        assertEquals(".appconfig.featureflag/" + id, f.getKey());
        f.setEnabled(false);
        client.setConfigurationSetting(f);
        assertFalse(((FeatureFlagConfigurationSetting) client.getConfigurationSetting(f.getKey(), null)).isEnabled());
    }

    @Test void secretReference_roundTripsAsASecretReferenceSetting() {
        String k = key();
        SecretReferenceConfigurationSetting ref = new SecretReferenceConfigurationSetting(k, "https://v.vault.azure.net/secrets/s");
        client.setConfigurationSetting(ref);
        ConfigurationSetting got = client.getConfigurationSetting(k, null);
        assertInstanceOf(SecretReferenceConfigurationSetting.class, got);
        assertEquals("https://v.vault.azure.net/secrets/s", ((SecretReferenceConfigurationSetting) got).getSecretId());
    }

    @Test void snapshot_createReadListAndArchive() {
        String p = key();
        client.setConfigurationSetting(p + "-a", null, "1");
        client.setConfigurationSetting(p + "-b", null, "2");
        String name = Emu.dashed("snap");
        com.azure.core.util.polling.SyncPoller<com.azure.core.util.polling.PollOperationDetails, ConfigurationSnapshot> poller = client.beginCreateSnapshot(name,
            new ConfigurationSnapshot(List.of(new ConfigurationSettingsFilter(p + "*"))).setRetentionPeriod(Duration.ofDays(7)), Context.NONE);
        poller.waitForCompletion(Duration.ofSeconds(60));
        ConfigurationSnapshot snap = poller.getFinalResult();
        assertEquals(name, snap.getName());
        assertEquals(ConfigurationSnapshotStatus.READY, snap.getStatus());
        assertEquals(2L, snap.getItemCount());
        // later changes do not show in the snapshot
        client.setConfigurationSetting(p + "-a", null, "changed");
        client.setConfigurationSetting(p + "-c", null, "3");
        List<ConfigurationSetting> items = client.listConfigurationSettingsForSnapshot(name).stream().collect(Collectors.toList());
        assertEquals(Set.of("1", "2"), items.stream().map(ConfigurationSetting::getValue).collect(Collectors.toSet()));
        assertEquals(ConfigurationSnapshotStatus.ARCHIVED, client.archiveSnapshot(name).getStatus());
        assertEquals(ConfigurationSnapshotStatus.READY, client.recoverSnapshot(name).getStatus());
        assertTrue(client.listSnapshots(new SnapshotSelector()).stream().anyMatch(s -> s.getName().equals(name)));
    }

    @Test void valueWithUnicodeAndLargeSize_roundTrips() {
        String k = key();
        String v = "日本語 \"quoted\" \n" + "z".repeat(50_000);
        client.setConfigurationSetting(k, null, v);
        assertEquals(v, client.getConfigurationSetting(k, null).getValue());
    }

    @Test void keyWithSlashAndSpecialCharacters_roundTrips() {
        String k = "app/" + key() + "/ä b+c";
        client.setConfigurationSetting(k, "l/1", "v");
        assertEquals("v", client.getConfigurationSetting(k, "l/1").getValue());
        assertTrue(client.listConfigurationSettings(new SettingSelector().setKeyFilter(k)).stream().anyMatch(s -> s.getKey().equals(k)));
    }
}
