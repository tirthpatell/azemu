//! Which documents a request addresses: one logical partition (header
//! `x-ms-documentdb-partitionkey`), an effective partition key range (`x-ms-start-epk` /
//! `x-ms-end-epk`), a physical partition (`x-ms-documentdb-partitionkeyrangeid`) or all.

use crate::epk;
use crate::errors::{CosmosError, R};
use crate::out::Cx;
use crate::pk::{self, DocKey, PkComp, PkValue};
use crate::state::{Container, Doc};

pub struct Selector {
    /// Full key (exact partition) or a prefix of a hierarchical key.
    pub pk: Option<PkValue>,
    pub epk_min: Option<String>,
    pub epk_max: Option<String>,
}

/// The only physical partition id of a container.
pub const RANGE_ID: &str = "0";

fn gone() -> CosmosError {
    CosmosError::new(410, "Gone", "The requested partition key range does not exist. Refresh the partition key ranges.").sub(1002)
}

impl Selector {
    pub fn from_headers(cx: &Cx, c: &Container) -> R<Selector> {
        if let Some(id) = cx.header("x-ms-documentdb-partitionkeyrangeid") {
            // "0" or "0,..." style ids; anything but our single range is gone
            if id != RANGE_ID {
                return Err(gone());
            }
        }
        let mut sel = Selector { pk: None, epk_min: None, epk_max: None };
        if let Some(raw) = cx.header("x-ms-documentdb-partitionkey") {
            let v = pk::parse_header(raw)?;
            let n = c.pk.paths.len();
            let ok = v.len() == n || (c.pk.hierarchical() && !v.is_empty() && v.len() < n) || (v.is_empty() && n == 1);
            if !ok {
                return Err(CosmosError::bad_request(
                    "The partition key supplied in x-ms-partitionkey header has fewer components than defined in the the collection.",
                )
                .sub(1001));
            }
            sel.pk = Some(if v.is_empty() { vec![PkComp::Undefined] } else { v });
        }
        if let Some(m) = cx.header("x-ms-start-epk") {
            sel.epk_min = Some(m.to_ascii_uppercase());
        }
        if let Some(m) = cx.header("x-ms-end-epk") {
            sel.epk_max = Some(m.to_ascii_uppercase());
        }
        Ok(sel)
    }

    pub fn matches(&self, key: &DocKey, d: &Doc) -> bool {
        self.matches_parts(key, &d.epk)
    }

    /// Same, for a key and its effective partition key (the change log keeps those apart from the document).
    pub fn matches_parts(&self, key: &DocKey, epk: &str) -> bool {
        if let Some(p) = &self.pk {
            if key.pk.len() < p.len() || key.pk[..p.len()] != p[..] {
                return false;
            }
        }
        epk::in_range(epk, self.epk_min.as_deref(), self.epk_max.as_deref())
    }

    pub fn docs<'a>(&'a self, c: &'a Container) -> Vec<&'a Doc> {
        let mut out: Vec<&Doc> = match &self.pk {
            Some(p) => c.partition(p).map(|(_, d)| d).collect(),
            None => c.docs.values().collect(),
        };
        if self.epk_min.is_some() || self.epk_max.is_some() {
            out.retain(|d| epk::in_range(&d.epk, self.epk_min.as_deref(), self.epk_max.as_deref()));
        }
        out
    }
}
