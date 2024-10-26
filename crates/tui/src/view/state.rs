//! State types for the view.

pub mod fixed_select;
pub mod select;

use chrono::{DateTime, Utc};
use derive_more::Deref;
use std::cell::{Ref, RefCell};

/// An internally mutable cell for UI state. Certain state needs to be updated
/// during the draw phase, typically because it's derived from parent data
/// passed via props. This is safe to use in the render phase, because rendering
/// is entirely synchronous.
///
/// In addition to storing the state value, this stores a state key as well. The
/// key is used to determine when to update the state. The key should be
/// something cheaply comparable. If the value itself is cheaply comparable,
/// you can just use that as the key.
#[derive(Debug)]
pub struct StateCell<K, V> {
    state: RefCell<Option<(K, V)>>,
}

impl<K, V> StateCell<K, V> {
    /// Get the current state value, or a new value if the state is stale. State
    /// will be stale if it is uninitialized OR the key has changed. In either
    /// case, `init` will be called to create a new value. The given key will be
    /// cloned iff the state is updated, so that the key can be stored.
    pub fn get_or_update(&self, key: &K, init: impl FnOnce() -> V) -> Ref<'_, V>
    where
        K: Clone + PartialEq,
    {
        let mut state = self.state.borrow_mut();
        match state.deref() {
            Some(state) if &state.0 == key => {}
            _ => {
                // (Re)create the state
                *state = Some((key.clone(), init()));
            }
        }
        drop(state);

        // Unwrap is safe because we just stored a value
        // It'd be nice to return an `impl Deref` here instead to prevent
        // leaking implementation details, but I was struggling with the
        // lifetimes on that
        Ref::map(self.state.borrow(), |state| &state.as_ref().unwrap().1)
    }

    /// Get a reference to the state key. This can panic, if the state is
    /// already borrowed elsewhere. Returns `None` iff the state cell is
    /// uninitialized.
    pub fn get_key(&self) -> Option<Ref<'_, K>> {
        Ref::filter_map(self.state.borrow(), |state| {
            state.as_ref().map(|(k, _)| k)
        })
        .ok()
    }

    /// Get a reference to the state value. This can panic, if the state  is
    /// already borrowed elsewhere. Returns `None` iff the state cell is
    /// uninitialized.
    pub fn get(&self) -> Option<Ref<'_, V>> {
        Ref::filter_map(self.state.borrow(), |state| {
            state.as_ref().map(|(_, v)| v)
        })
        .ok()
    }

    /// Get a mutable reference to the state value. This will never panic
    /// because `&mut self` guarantees exclusive access. Returns `None` iff
    /// the state cell is uninitialized.
    pub fn get_mut(&mut self) -> Option<&mut V> {
        self.state.get_mut().as_mut().map(|state| &mut state.1)
    }
}

/// Derive impl applies unnecessary bound on the generic parameter
impl<K, V> Default for StateCell<K, V> {
    fn default() -> Self {
        Self {
            state: RefCell::new(None),
        }
    }
}

/// A notification is an ephemeral informational message generated by some async
/// action. It doesn't grab focus, but will be useful to the user nonetheless.
/// It should be shown for a short period of time, then disappear on its own.
#[derive(Debug)]
pub struct Notification {
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

impl Notification {
    pub fn new(message: String) -> Self {
        Self {
            message,
            timestamp: Utc::now(),
        }
    }
}