// SPDX-License-Identifier: Apache-2.0
//! Lookup from class and property IRIs to the triples maps that produce them.

use std::collections::HashMap;

use super::{Mapping, TriplesMapId};

/// One way a property can be produced: a predicate-object map of a
/// triples map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyCandidate {
    pub triples_map: TriplesMapId,
    /// Position in [`TriplesMap::predicate_object_maps`](super::TriplesMap).
    pub predicate_object_map: usize,
}

/// Class and property IRIs mapped to their candidate triples maps.
///
/// Keys and candidates keep the order they first appear in the mapping, so
/// that everything generated from the index (SQL, `terms` output) is stable
/// across runs.
#[derive(Debug, Clone, Default)]
pub struct MappingIndex {
    classes: OrderedMap<TriplesMapId>,
    properties: OrderedMap<PropertyCandidate>,
}

impl MappingIndex {
    pub fn new(mapping: &Mapping) -> Self {
        let mut index = Self::default();
        for tm in mapping.triples_maps() {
            for class in &tm.subject_map.classes {
                index.classes.push(class, tm.id.clone());
            }
            for (position, pom) in tm.predicate_object_maps.iter().enumerate() {
                for predicate in &pom.predicates {
                    index.properties.push(
                        predicate,
                        PropertyCandidate {
                            triples_map: tm.id.clone(),
                            predicate_object_map: position,
                        },
                    );
                }
            }
        }
        index
    }

    /// Triples maps whose subjects have `rdf:type <class>`.
    pub fn class(&self, class: &str) -> &[TriplesMapId] {
        self.classes.get(class)
    }

    /// Predicate-object maps that produce `<property>`.
    pub fn property(&self, property: &str) -> &[PropertyCandidate] {
        self.properties.get(property)
    }

    pub fn classes(&self) -> impl Iterator<Item = (&str, &[TriplesMapId])> {
        self.classes.iter()
    }

    pub fn properties(&self) -> impl Iterator<Item = (&str, &[PropertyCandidate])> {
        self.properties.iter()
    }
}

#[derive(Debug, Clone)]
struct OrderedMap<V> {
    entries: Vec<(String, Vec<V>)>,
    positions: HashMap<String, usize>,
}

impl<V> Default for OrderedMap<V> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            positions: HashMap::new(),
        }
    }
}

impl<V> OrderedMap<V> {
    fn push(&mut self, key: &str, value: V) {
        let position = *self.positions.entry(key.to_owned()).or_insert_with(|| {
            self.entries.push((key.to_owned(), Vec::new()));
            self.entries.len() - 1
        });
        self.entries[position].1.push(value);
    }

    fn get(&self, key: &str) -> &[V] {
        self.positions
            .get(key)
            .map_or(&[], |&position| &self.entries[position].1)
    }

    fn iter(&self) -> impl Iterator<Item = (&str, &[V])> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }
}
