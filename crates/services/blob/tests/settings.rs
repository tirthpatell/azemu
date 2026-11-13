//! The settings of the Blob service. One test: the settings are process-wide, so everything that
//! sets one runs in this single function, in order.

use azemu_blob::BlobService;
use azemu_core::settings;

#[test]
fn versioning_is_off_by_default_and_the_switches_work() {
    assert!(
        std::env::var_os("AZEMU_LENIENT").is_none() && std::env::var_os("AZEMU_BLOB_VERSIONING").is_none(),
        "run this test with AZEMU_LENIENT and AZEMU_BLOB_VERSIONING unset: it checks the defaults"
    );
    // the default: an account that appears on first use keeps no versions, as on Azure
    let svc = BlobService::new();
    assert!(!svc.versioning_enabled("fresh"));
    let in_force = |name: &str| settings::in_force().into_iter().find(|s| s.name == name).map(|s| (s.value, s.source));
    assert_eq!(in_force("AZEMU_BLOB_VERSIONING"), Some(("false".into(), "default")));

    // one account can be switched on (ARM does this) without touching the others
    svc.set_versioning_enabled("fresh", true);
    assert!(svc.versioning_enabled("fresh") && !svc.versioning_enabled("other"));

    // the lenient switch turns it on for accounts that appear later
    settings::set_override("AZEMU_LENIENT", "1");
    let svc = BlobService::new();
    assert!(svc.versioning_enabled("fresh"));
    assert_eq!(in_force("AZEMU_BLOB_VERSIONING"), Some(("true".into(), "lenient")));

    // the variable of its own beats the lenient switch, either way
    settings::set_override("AZEMU_BLOB_VERSIONING", "off");
    assert!(!BlobService::new().versioning_enabled("fresh"));
    settings::set_override("AZEMU_LENIENT", "0");
    settings::set_override("AZEMU_BLOB_VERSIONING", "yes");
    assert!(BlobService::new().versioning_enabled("fresh"));

    // every setting of the crate is described for docs/SETTINGS.md
    let described: Vec<String> = settings::described().into_iter().map(|d| d.name).collect();
    for name in [
        "AZEMU_BLOB_VERSIONING",
        "AZEMU_BLOB_VERSION_BYTES",
        "AZEMU_BLOB_SOFT_DELETE_BYTES",
        "AZEMU_BLOB_QUERY_MAX_BLOB_BYTES",
        "AZEMU_BLOB_QUERY_SECONDS",
        "AZEMU_BLOB_QUERY_MAX_CONCURRENT",
    ] {
        assert!(described.iter().any(|d| d == name), "{name} is not described");
    }
}
