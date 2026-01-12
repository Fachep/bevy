//! Centralized storage for observers, allowing for efficient look-ups.
//!
//! This has multiple levels:
//! - [`World::observers`](crate::world::World::observers) provides access to [`Observers`], which is a central storage for all observers.
//! - [`Observers`] contains multiple distinct caches in the form of [`CachedObservers`].
//!     - Most observers are looked up by the [`ComponentId`] of the event they are observing
//!     - Lifecycle observers have their own fields to save lookups.
//! - [`CachedObservers`] contains maps of [`ObserverRunner`]s, which are the actual functions that will be run when the observer is triggered.
//!     - These are split by target type, in order to allow for different lookup strategies.
//!     - [`CachedComponentObservers`] is one of these maps, which contains observers that are specifically targeted at a component.

use alloc::boxed::Box;
use bevy_platform::collections::HashMap;
use bevy_utils::prelude::DebugName;
use crate::{
    archetype::ArchetypeFlags,
    component::ComponentId,
    entity::EntityHashMap,
    event::{Event, EventKey},
    observer::{consumer_system_runner, AnyNamedSystem, ConsumerRunner, ObserverRunner},
    system::{IntoConsumerSystem, IntoSystem, System},
};

/// An internal lookup table tracking all of the observers in the world.
///
/// Stores a cache mapping event ids to their registered observers.
/// Some observer kinds (like [lifecycle](crate::lifecycle) observers) have a dedicated field,
/// saving lookups for the most common triggers.
///
/// This can be accessed via [`World::observers`](crate::world::World::observers).
#[derive(Default, Debug)]
pub struct Observers {
    // Cached ECS observers to save a lookup for high-traffic built-in event types.
    add: CachedObservers,
    insert: CachedObservers,
    replace: CachedObservers,
    remove: CachedObservers,
    despawn: CachedObservers,
    // Map from event type to set of observers watching for that event
    cache: HashMap<EventKey, CachedObservers>,
    consumers: HashMap<EventKey, Consumer>,
}

impl Observers {
    pub(crate) fn get_observers_mut(&mut self, event_key: EventKey) -> &mut CachedObservers {
        use crate::lifecycle::*;

        match event_key {
            ADD => &mut self.add,
            INSERT => &mut self.insert,
            REPLACE => &mut self.replace,
            REMOVE => &mut self.remove,
            DESPAWN => &mut self.despawn,
            _ => self.cache.entry(event_key).or_default(),
        }
    }

    /// Attempts to get the observers for the given `event_key`.
    ///
    /// When accessing the observers for lifecycle events, such as [`Add`], [`Insert`], [`Replace`], [`Remove`], and [`Despawn`],
    /// use the [`EventKey`] constants from the [`lifecycle`](crate::lifecycle) module.
    ///
    /// [`Add`]: crate::lifecycle::Add
    /// [`Insert`]: crate::lifecycle::Insert
    /// [`Replace`]: crate::lifecycle::Replace
    /// [`Remove`]: crate::lifecycle::Remove
    /// [`Despawn`]: crate::lifecycle::Despawn
    pub fn try_get_observers(&self, event_key: EventKey) -> Option<&CachedObservers> {
        use crate::lifecycle::*;

        match event_key {
            ADD => Some(&self.add),
            INSERT => Some(&self.insert),
            REPLACE => Some(&self.replace),
            REMOVE => Some(&self.remove),
            DESPAWN => Some(&self.despawn),
            _ => self.cache.get(&event_key),
        }
    }

    pub(crate) fn is_archetype_cached(event_key: EventKey) -> Option<ArchetypeFlags> {
        use crate::lifecycle::*;

        match event_key {
            ADD => Some(ArchetypeFlags::ON_ADD_OBSERVER),
            INSERT => Some(ArchetypeFlags::ON_INSERT_OBSERVER),
            REPLACE => Some(ArchetypeFlags::ON_REPLACE_OBSERVER),
            REMOVE => Some(ArchetypeFlags::ON_REMOVE_OBSERVER),
            DESPAWN => Some(ArchetypeFlags::ON_DESPAWN_OBSERVER),
            _ => None,
        }
    }

    pub(crate) fn update_archetype_flags(
        &self,
        component_id: ComponentId,
        flags: &mut ArchetypeFlags,
    ) {
        if self.add.component_observers.contains_key(&component_id) {
            flags.insert(ArchetypeFlags::ON_ADD_OBSERVER);
        }

        if self.insert.component_observers.contains_key(&component_id) {
            flags.insert(ArchetypeFlags::ON_INSERT_OBSERVER);
        }

        if self.replace.component_observers.contains_key(&component_id) {
            flags.insert(ArchetypeFlags::ON_REPLACE_OBSERVER);
        }

        if self.remove.component_observers.contains_key(&component_id) {
            flags.insert(ArchetypeFlags::ON_REMOVE_OBSERVER);
        }

        if self.despawn.component_observers.contains_key(&component_id) {
            flags.insert(ArchetypeFlags::ON_DESPAWN_OBSERVER);
        }
    }

    /// Attempts to get the consumer for the given `event_key`.
    pub fn try_get_consumer(&self, event_key: EventKey) -> Option<&Consumer> {
        self.consumers.get(&event_key)
    }

    pub(crate) fn try_get_consumer_mut(&mut self, event_key: EventKey) -> Option<&mut Consumer> {
        self.consumers.get_mut(&event_key)
    }

    pub(crate) fn try_set_consumer(
        &mut self,
        event_key: EventKey,
        consumer: Consumer,
    ) -> Result<&mut Consumer, Consumer> {
        self.consumers
            .try_insert(event_key, consumer)
            .map_err(|e| e.value)
    }
}

/// Collection of [`ObserverRunner`] for [`Observer`](crate::observer::Observer) registered to a particular event.
///
/// This is stored inside of [`Observers`], specialized for each kind of observer.
#[derive(Default, Debug)]
pub struct CachedObservers {
    /// Observers watching for any time this event is triggered, regardless of target.
    /// These will also respond to events targeting specific components or entities
    pub(super) global_observers: ObserverMap,
    /// Observers watching for triggers of events for a specific component
    pub(super) component_observers: HashMap<ComponentId, CachedComponentObservers>,
    /// Observers watching for triggers of events for a specific entity
    pub(super) entity_observers: EntityHashMap<ObserverMap>,
}

impl CachedObservers {
    /// Observers watching for any time this event is triggered, regardless of target.
    /// These will also respond to events targeting specific components or entities
    pub fn global_observers(&self) -> &ObserverMap {
        &self.global_observers
    }

    /// Returns observers watching for triggers of events for a specific component.
    pub fn component_observers(&self) -> &HashMap<ComponentId, CachedComponentObservers> {
        &self.component_observers
    }

    /// Returns observers watching for triggers of events for a specific entity.
    pub fn entity_observers(&self) -> &EntityHashMap<ObserverMap> {
        &self.entity_observers
    }
}

/// Map between an observer entity and its [`ObserverRunner`]
pub type ObserverMap = EntityHashMap<ObserverRunner>;

/// Collection of [`ObserverRunner`] for [`Observer`](crate::observer::Observer) registered to a particular event targeted at a specific component.
///
/// This is stored inside of [`CachedObservers`].
#[derive(Default, Debug)]
pub struct CachedComponentObservers {
    // Observers watching for events targeting this component, but not a specific entity
    pub(super) global_observers: ObserverMap,
    // Observers watching for events targeting this component on a specific entity
    pub(super) entity_component_observers: EntityHashMap<ObserverMap>,
}

impl CachedComponentObservers {
    /// Returns observers watching for events targeting this component, but not a specific entity
    pub fn global_observers(&self) -> &ObserverMap {
        &self.global_observers
    }

    /// Returns observers watching for events targeting this component on a specific entity
    pub fn entity_component_observers(&self) -> &EntityHashMap<ObserverMap> {
        &self.entity_component_observers
    }
}

/// A [`Consumer`] System.
///
/// Consumer runs after all observers have been triggered for a given event.
pub struct Consumer {
    pub(crate) system: Box<dyn AnyNamedSystem>,
    pub(crate) runner: ConsumerRunner,
}

impl Consumer {
    /// Creates a new [`Consumer`] from the given system.
    ///
    /// # Panics
    ///
    /// Panics if the given system is an exclusive system.
    pub fn new<E: Event, M, I: IntoConsumerSystem<E, M>>(system: I) -> Self {
        let system = Box::new(IntoConsumerSystem::into_system(system));
        assert!(
            !system.is_exclusive(),
            concat!(
            "Exclusive system `{}` may not be used as consumer.\n",
            "Instead of `&mut World`, use either `DeferredWorld` if you do not need structural changes, or `Commands` if you do."
            ),
            system.name()
        );
        Self {
            system,
            runner: consumer_system_runner::<E, I::System>,
        }
    }

    /// Creates a new [`Consumer`] with custom runner
    pub fn with_dynamic_runner(runner: ConsumerRunner) -> Self {
        Self {
            system: Box::new(IntoSystem::into_system(|| {})),
            runner,
        }
    }

    /// Returns the name of the [`Consumer`]'s system .
    pub fn system_name(&self) -> DebugName {
        self.system.system_name()
    }
}

impl core::fmt::Debug for Consumer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Consumer")
            .field("system", &self.system.system_name())
            .field("runner", &self.runner)
            .finish()
    }
}
