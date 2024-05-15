//! The fieldids weights map is in charge of storing linking the searchable fields with their weights.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{FieldId, Weight};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FieldidsWeightsMap {
    map: HashMap<FieldId, Weight>,
}

impl FieldidsWeightsMap {
    /// Insert a field id -> weigth into the map.
    /// If the map did not have this key present, `None` is returned.
    /// If the map did have this key present, the value is updated, and the old value is returned.
    pub fn insert(&mut self, fid: FieldId, weight: Weight) -> Option<Weight> {
        self.map.insert(fid, weight)
    }

    /// Removes a field id from the map, returning the associated weight previously in the map.
    pub fn remove(&mut self, fid: FieldId) -> Option<Weight> {
        self.map.remove(&fid)
    }

    /// Returns weight corresponding to the key.
    pub fn weight(&self, fid: FieldId) -> Option<Weight> {
        self.map.get(&fid).copied()
    }

    /// Returns highest weight contained in the map if any.
    pub fn max_weight(&self) -> Option<Weight> {
        self.map.values().copied().max()
    }

    /// Return an iterator visiting all field ids in arbitrary order.
    pub fn ids(&self) -> impl Iterator<Item = FieldId> + '_ {
        self.map.keys().copied()
    }
}
