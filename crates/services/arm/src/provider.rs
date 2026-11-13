//! The interface between the generic ARM engine and a resource provider that is
//! wired to an emulated service.

use azemu_core::{async_trait, Response};
use serde_json::Value;

use crate::cx::Cx;
use crate::error::ArmResult;
use crate::state::Stored;

/// Status codes a provider answers with. The values come from the REST API specs.
#[derive(Clone, Copy)]
pub struct Statuses {
    pub create: u16,
    pub update: u16,
    /// DELETE of a resource that existed.
    pub delete: u16,
    /// DELETE of a resource that did not exist.
    pub delete_missing: u16,
}

pub const STATUSES_DEFAULT: Statuses = Statuses { create: 201, update: 200, delete: 200, delete_missing: 204 };

pub enum ActionTarget<'a> {
    /// `POST /subscriptions/{s}/providers/{ns}/{action}`
    Provider,
    Resource(&'a Stored),
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn namespace(&self) -> &'static str;

    /// Canonical type paths the provider builds, e.g. `storageAccounts/blobServices/containers`.
    fn types(&self) -> &'static [&'static str];

    /// Types stored as plain JSON because they carry settings only and need nothing from
    /// a service (for example `storageAccounts/fileServices`).
    fn passthrough(&self) -> &'static [&'static str] {
        &[]
    }

    /// Names that are POST actions but are also read with GET (`readonlykeys`).
    fn get_actions(&self) -> &'static [&'static str] {
        &[]
    }

    fn statuses(&self, _type_path: &str) -> Statuses {
        STATUSES_DEFAULT
    }

    /// Names are unique across the whole emulator (they are host names).
    fn global_name(&self, type_path: &str) -> bool {
        !type_path.contains('/')
    }

    /// Top-level resources need `location`.
    fn requires_location(&self, type_path: &str) -> bool {
        !type_path.contains('/')
    }

    fn validate_name(&self, _type_path: &str, _name: &str) -> ArmResult<()> {
        Ok(())
    }

    /// Called with the request body already merged into `res.body`. May reject, fill in
    /// defaults, hidden state, and create things in the emulated service. `children` receives
    /// resources to store along with this one (for example `blobServices/default`).
    async fn put(&self, cx: &Cx<'_>, res: &mut Stored, existed: bool, children: &mut Vec<Stored>) -> ArmResult<()>;

    /// The resource is being removed (also called for each member of a deleted group). An error means the data plane did not
    /// remove it: ARM then keeps the resource and answers the error (a delete either removes it or says why not). `force` is set
    /// when it goes away because its parent or its group is deleted: Azure drops everything below it then (a lease on a container
    /// does not stop the delete of its account).
    async fn delete(&self, cx: &Cx<'_>, res: &Stored, force: bool) -> ArmResult<()>;

    /// What the client sees. Adds the fields that depend on the request (URLs with the port).
    fn render(&self, _cx: &Cx<'_>, res: &Stored) -> Value {
        res.body.clone()
    }

    /// POST actions. `Ok(None)` means "not built": the engine answers 501 naming it.
    async fn action(
        &self,
        cx: &Cx<'_>,
        target: ActionTarget<'_>,
        type_path: &str,
        action: &str,
        body: &Value,
    ) -> ArmResult<Option<Response>>;
}
