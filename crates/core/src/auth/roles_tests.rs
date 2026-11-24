//! Tests of the registry (`roles.rs`): wildcards, `notDataActions`, scope inheritance, several assignments, custom roles, removal.
use super::*;

const ACCT_ID: &str = "/subscriptions/s1/resourceGroups/g1/providers/Microsoft.Storage/storageAccounts/acct";
const P: &str = "11111111-1111-1111-1111-111111111111";
const Q: &str = "22222222-2222-2222-2222-222222222222";
const READER: &str = "2a2b9908-6ea1-4ae2-8e65-a410df84e7d1";
const CONTRIBUTOR_ROLE: &str = "b24988ac-6180-42a0-ab88-20f7382dd24c";

fn assign(name: &str, scope: &str, role: &str, principal: &str) -> Assignment {
    Assignment {
        id: name.into(),
        scope: scope.into(),
        role_id: role.into(),
        principal_id: principal.into(),
        principal_type: "ServicePrincipal".into(),
        condition: None,
        condition_version: None,
        description: None,
        created_on: String::new(),
        updated_on: String::new(),
        origin: Origin::Api,
    }
}

fn read_blob() -> Need {
    Need::data("Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read")
}

fn registry() -> Registry {
    let r = Registry::new();
    r.register_resource("Microsoft.Storage/storageAccounts", "acct", ACCT_ID);
    r
}

#[test]
fn nothing_is_granted_without_an_assignment() {
    let r = registry();
    let d = r.check(P, &read_blob(), &Resource::container("acct", "c"));
    assert!(!d.allowed);
    assert!(d.reason.contains("no role assignment at all"), "{}", d.reason);
}

#[test]
fn data_reader_reads_blobs_but_does_not_write() {
    let r = registry();
    r.put_assignment(assign("a1", "/subscriptions/s1/resourceGroups/g1", READER, P)).unwrap();
    let res = Resource::container("acct", "c");
    assert!(r.may(P, &read_blob(), &res));
    assert!(!r.may(P, &Need::data("Microsoft.Storage/storageAccounts/blobServices/containers/blobs/write"), &res));
    assert!(!r.may(Q, &read_blob(), &res));
}

#[test]
fn contributor_has_actions_and_no_data_actions() {
    let r = registry();
    r.put_assignment(assign("a1", ACCT_ID, CONTRIBUTOR_ROLE, P)).unwrap();
    let res = Resource::container("acct", "c");
    assert!(!r.may(P, &read_blob(), &res));
    assert!(r.may(P, &Need::control("Microsoft.Storage/storageAccounts/blobServices/containers/read"), &res));
}

#[test]
fn a_scope_on_one_container_does_not_reach_the_account_or_another_container() {
    let r = registry();
    let scope = format!("{ACCT_ID}/blobServices/default/containers/c1");
    r.put_assignment(assign("a1", &scope, READER, P)).unwrap();
    assert!(r.may(P, &read_blob(), &Resource::container("acct", "c1")));
    assert!(!r.may(P, &read_blob(), &Resource::container("acct", "c2")));
    assert!(!r.may(P, &Need::data("x"), &Resource::storage("acct")));
}

#[test]
fn assignments_add_up() {
    let r = registry();
    // the queue reader on the queue, the blob reader on the account
    r.put_assignment(assign("a1", ACCT_ID, READER, P)).unwrap();
    r.put_assignment(assign("a2", &format!("{ACCT_ID}/queueServices/default/queues/q"), "19e7f393-937e-4f77-808e-94535e297925", P))
        .unwrap();
    assert!(r.may(P, &read_blob(), &Resource::container("acct", "c")));
    assert!(r.may(P, &Need::data("Microsoft.Storage/storageAccounts/queueServices/queues/messages/read"), &Resource::queue("acct", "q")));
    assert!(!r.may(
        P,
        &Need::data("Microsoft.Storage/storageAccounts/queueServices/queues/messages/read"),
        &Resource::queue("acct", "other")
    ));
}

#[test]
fn any_and_all_of_several_actions() {
    let r = registry();
    // Message Processor: read + process
    r.put_assignment(assign("a1", ACCT_ID, "8a0f0c08-91a1-4084-bc3d-661d67233fed", P)).unwrap();
    let q = Resource::queue("acct", "q");
    let base = "Microsoft.Storage/storageAccounts/queueServices/queues/messages";
    let get_messages = Need::any(vec![
        Need::data(&format!("{base}/process/action")),
        Need::all(vec![Need::data(&format!("{base}/delete")), Need::data(&format!("{base}/read"))]),
    ]);
    assert!(r.may(P, &get_messages, &q));
    let all = Need::all(vec![Need::data(&format!("{base}/delete")), Need::data(&format!("{base}/read"))]);
    assert!(!r.may(P, &all, &q));
}

#[test]
fn not_data_actions_take_away_from_their_own_role_only() {
    let r = registry();
    let (custom, _) = r
        .put_role(RoleDef {
            id: "33333333-3333-3333-3333-333333333333".into(),
            name: "Blob without delete".into(),
            description: String::new(),
            builtin: false,
            actions: vec![],
            not_actions: vec![],
            data_actions: vec!["Microsoft.Storage/storageAccounts/blobServices/containers/blobs/*".into()],
            not_data_actions: vec!["Microsoft.Storage/storageAccounts/blobServices/containers/blobs/delete".into()],
            assignable_scopes: vec!["/".into()],
            created_on: String::new(),
            updated_on: String::new(),
        })
        .unwrap();
    r.put_assignment(assign("a1", ACCT_ID, &custom.id, P)).unwrap();
    let res = Resource::container("acct", "c");
    let del = Need::data("Microsoft.Storage/storageAccounts/blobServices/containers/blobs/delete");
    assert!(r.may(P, &read_blob(), &res));
    assert!(!r.may(P, &del, &res));
    // a second role that does grant delete adds to it
    r.put_assignment(assign("a2", ACCT_ID, "ba92f5b4-2d11-453d-a403-e96b0029c9fe", P)).unwrap();
    assert!(r.may(P, &del, &res));
}

#[test]
fn a_custom_role_with_an_unknown_action_is_fine() {
    let r = registry();
    let d = RoleDef {
        id: "44444444-4444-4444-4444-444444444444".into(),
        name: "odd".into(),
        description: String::new(),
        builtin: false,
        actions: vec![],
        not_actions: vec![],
        data_actions: vec!["Contoso.Thing/frobnicate/action".into()],
        not_data_actions: vec![],
        assignable_scopes: vec![],
        created_on: String::new(),
        updated_on: String::new(),
    };
    let (d, _) = r.put_role(d).unwrap();
    r.put_assignment(assign("a1", ACCT_ID, &d.id, P)).unwrap();
    assert!(!r.may(P, &read_blob(), &Resource::container("acct", "c")));
    assert!(r.may(P, &Need::data("Contoso.Thing/frobnicate/action"), &Resource::storage("acct")));
}

#[test]
fn duplicate_and_conflicting_assignments() {
    let r = registry();
    r.put_assignment(assign("a1", ACCT_ID, READER, P)).unwrap();
    // the same name and content again is not an error
    assert!(!r.put_assignment(assign("a1", ACCT_ID, READER, P)).unwrap().1);
    // another name, same principal, role and scope
    assert_eq!(r.put_assignment(assign("a2", ACCT_ID, READER, P)), Err(AssignError::Exists));
    // the name of another principal
    assert_eq!(r.put_assignment(assign("a1", ACCT_ID, READER, Q)), Err(AssignError::UpdateNotPermitted));
    assert!(matches!(r.put_assignment(assign("a3", ACCT_ID, "99999999-9999-9999-9999-999999999999", P)), Err(AssignError::UnknownRole(_))));
}

#[test]
fn removal_takes_the_rights_away() {
    let r = registry();
    r.put_assignment(assign("a1", ACCT_ID, READER, P)).unwrap();
    assert!(r.may(P, &read_blob(), &Resource::container("acct", "c")));
    assert!(r.delete_assignment(ACCT_ID, "a1").is_some());
    assert!(!r.may(P, &read_blob(), &Resource::container("acct", "c")));
    assert!(r.delete_assignment(ACCT_ID, "a1").is_none());
}

#[test]
fn a_role_in_use_cannot_be_deleted_and_a_built_in_one_never() {
    let r = registry();
    let d = RoleDef {
        id: "55555555-5555-5555-5555-555555555555".into(),
        name: "mine".into(),
        description: String::new(),
        builtin: false,
        actions: vec![],
        not_actions: vec![],
        data_actions: vec![],
        not_data_actions: vec![],
        assignable_scopes: vec![],
        created_on: String::new(),
        updated_on: String::new(),
    };
    let (d, _) = r.put_role(d).unwrap();
    r.put_assignment(assign("a1", ACCT_ID, &d.id, P)).unwrap();
    assert_eq!(r.delete_role(&d.id), Err(RoleError::InUse));
    r.delete_assignment(ACCT_ID, "a1");
    assert!(r.delete_role(&d.id).is_ok());
    assert_eq!(r.delete_role(READER), Err(RoleError::ReadOnly));
}

#[test]
fn an_account_not_made_through_arm_is_covered_by_a_scope_that_names_it_only() {
    let r = Registry::new();
    r.put_assignment(assign("a1", "/subscriptions/s1/resourceGroups/g1", READER, P)).unwrap();
    assert!(!r.may(P, &read_blob(), &Resource::container("acct", "c")));
    r.put_assignment(assign("a2", &super::super::scope::local_id(&Resource::storage("acct")), READER, Q)).unwrap();
    assert!(r.may(Q, &read_blob(), &Resource::container("acct", "c")));
    let d = r.check(P, &read_blob(), &Resource::container("acct", "c"));
    assert_eq!(d.looked_at.len(), 1);
    assert!(!d.looked_at[0].applies);
}

#[test]
fn roles_are_found_by_name_by_id_and_by_full_id() {
    let r = Registry::new();
    assert_eq!(r.find_role("storage blob data reader").unwrap().id, READER);
    assert_eq!(r.find_role(READER).unwrap().name, "Storage Blob Data Reader");
    assert_eq!(r.find_role(&format!("/subscriptions/s/providers/Microsoft.Authorization/roleDefinitions/{READER}")).unwrap().id, READER);
    assert!(r.find_role("nope").is_none());
}

#[test]
fn cosmos_roles_follow_the_scope_path() {
    let r = Registry::new();
    r.put_cosmos_assignment(
        "acct",
        CosmosAssignment {
            id: "x1".into(),
            role_id: "00000000-0000-0000-0000-000000000001".into(),
            principal_id: P.into(),
            scope: "/subscriptions/s/resourceGroups/g/providers/Microsoft.DocumentDB/databaseAccounts/acct/dbs/d1".into(),
            origin: Origin::Api,
        },
    );
    let read = "Microsoft.DocumentDB/databaseAccounts/sqlDatabases/containers/items/read";
    assert!(r.cosmos_check("acct", P, read, "/dbs/d1/colls/c").allowed);
    assert!(!r.cosmos_check("acct", P, read, "/dbs/d2/colls/c").allowed);
    assert!(
        !r.cosmos_check("acct", P, "Microsoft.DocumentDB/databaseAccounts/sqlDatabases/containers/items/create", "/dbs/d1/colls/c").allowed
    );
    assert!(!r.cosmos_check("acct", Q, read, "/dbs/d1").allowed);
}

#[test]
fn cosmos_database_and_container_names_are_case_sensitive_as_they_are_in_cosmos_db() {
    let r = Registry::new();
    let read = "Microsoft.DocumentDB/databaseAccounts/sqlDatabases/containers/items/read";
    for (i, scope) in
        ["/dbs/App", "/subscriptions/s/resourceGroups/g/providers/Microsoft.DocumentDB/databaseAccounts/Acct/dbs/App/colls/Items"]
            .iter()
            .enumerate()
    {
        r.put_cosmos_assignment(
            "Acct",
            CosmosAssignment {
                id: format!("x{i}"),
                role_id: "00000000-0000-0000-0000-000000000001".into(),
                principal_id: P.into(),
                scope: (*scope).into(),
                origin: Origin::Api,
            },
        );
    }
    // the account is a host name, so its case does not matter; the database and the container are names
    assert!(r.cosmos_check("acct", P, read, "/dbs/App/colls/anything").allowed);
    assert!(r.cosmos_check("ACCT", P, read, "/dbs/App/colls/Items").allowed);
    assert!(!r.cosmos_check("acct", P, read, "/dbs/app/colls/anything").allowed, "`app` is another database than `App`");
    assert!(!r.cosmos_check("acct", P, read, "/dbs/APP").allowed);
    assert_eq!(
        cosmos_path_of_scope("/subscriptions/S/resourceGroups/G/providers/Microsoft.DocumentDB/databaseAccounts/Acct/dbs/App/colls/Items/"),
        "/dbs/App/colls/Items"
    );
}

#[test]
fn save_and_restore_keep_what_is_saved_and_leave_the_rest() {
    let r = registry();
    r.put_assignment(assign("a1", ACCT_ID, READER, P)).unwrap();
    let mut f = assign("a2", ACCT_ID, READER, Q);
    f.origin = Origin::File;
    r.put_assignment(f).unwrap();
    let s = r.snapshot();
    assert_eq!(s.assignments.len(), 1);
    let r2 = Registry::new();
    r2.restore(s);
    assert_eq!(r2.assignments().len(), 1);
}
