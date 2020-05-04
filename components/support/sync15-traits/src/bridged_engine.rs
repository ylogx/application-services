/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};

use super::{Guid, Payload, ServerTimestamp};

/// A bridged Sync engine implements all the methods needed to support
/// Desktop Sync.
pub trait BridgedEngine {
    /// The type returned for errors.
    type Error;

    /// Initializes the engine. This is called once, when the engine is first
    /// created, and guaranteed to be called before any of the other methods.
    /// The default implementation does nothing.
    fn initialize(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Returns the last sync time, in milliseconds, for this engine's
    /// collection. This is called before each sync, to determine the lower
    /// bound for new records to fetch from the server.
    fn last_sync(&self) -> Result<i64, Self::Error>;

    /// Sets the last sync time, in milliseconds. This is called throughout
    /// the sync, to fast-forward the stored last sync time to match the
    /// timestamp on the uploaded records.
    fn set_last_sync(&self, last_sync_millis: i64) -> Result<(), Self::Error>;

    /// Returns the sync ID for this engine's collection. This is only used in
    /// tests.
    fn sync_id(&self) -> Result<Option<String>, Self::Error>;

    /// Resets the sync ID for this engine's collection, returning the new ID.
    /// As a side effect, implementations should reset all local Sync state,
    /// as in `reset`.
    fn reset_sync_id(&self) -> Result<String, Self::Error>;

    /// Ensures that the locally stored sync ID for this engine's collection
    /// matches the `new_sync_id` from the server. If the two don't match,
    /// implementations should reset all local Sync state, as in `reset`.
    /// This method returns the assigned sync ID, which can be either the
    /// `new_sync_id`, or a different one if the engine wants to force other
    /// devices to reset their Sync state for this collection the next time they
    /// sync.
    fn ensure_current_sync_id(&self, new_sync_id: &str) -> Result<String, Self::Error>;

    /// Stages a batch of incoming Sync records. This is called multiple
    /// times per sync, once for each batch. Implementations can use the
    /// signal to check if the operation was aborted, and cancel any
    /// pending work.
    fn store_incoming(&self, incoming_cleartexts: &[IncomingEnvelope]) -> Result<(), Self::Error>;

    /// Applies all staged records, reconciling changes on both sides and
    /// resolving conflicts. Returns a list of records to upload.
    fn apply(&self) -> Result<ApplyResults, Self::Error>;

    /// Indicates that the given record IDs were uploaded successfully to the
    /// server. This is called multiple times per sync, once for each batch
    /// upload.
    fn set_uploaded(&self, server_modified_millis: i64, ids: &[String]) -> Result<(), Self::Error>;

    /// Indicates that all records have been uploaded. At this point, any record
    /// IDs marked for upload that haven't been passed to `set_uploaded`, can be
    /// assumed to have failed: for example, because the server rejected a record
    /// with an invalid TTL or sort index.
    fn sync_finished(&self) -> Result<(), Self::Error>;

    /// Resets all local Sync state, including any change flags, mirrors, and
    /// the last sync time, such that the next sync is treated as a first sync
    /// with all new local data. Does not erase any local user data.
    fn reset(&self) -> Result<(), Self::Error>;

    /// Erases all local user data for this collection, and any Sync metadata.
    /// This method is destructive, and unused for most collections.
    fn wipe(&self) -> Result<(), Self::Error>;

    /// Tears down the engine. The opposite of `initialize`, `finalize` is
    /// called when an engine is disabled, or otherwise no longer needed. The
    /// default implementation does nothing.
    fn finalize(&self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct ApplyResults {
    /// List of records
    pub envelopes: Vec<OutgoingEnvelope>,
    /// The number of incoming records whose contents were merged because they
    /// changed on both sides. None indicates we aren't reporting this
    /// information.
    pub num_reconciled: Option<usize>,
}

impl ApplyResults {
    pub fn new(envelopes: Vec<OutgoingEnvelope>, num_reconciled: impl Into<Option<usize>>) -> Self {
        Self {
            envelopes,
            num_reconciled: num_reconciled.into(),
        }
    }
}

// Shorthand for engines that don't care.
impl From<Vec<OutgoingEnvelope>> for ApplyResults {
    fn from(envelopes: Vec<OutgoingEnvelope>) -> Self {
        Self {
            envelopes,
            num_reconciled: None,
        }
    }
}

/// An envelope for an incoming item, passed to `BridgedEngine::store_incoming`.
/// Envelopes are a halfway point between BSOs, the format used for all items on
/// the Sync server, and records, which are specific to each engine.
///
/// A BSO is a JSON object with metadata fields (`id`, `modifed`, `sortindex`),
/// and a BSO payload that is itself a JSON string. For encrypted records, the
/// BSO payload has a ciphertext, which must be decrypted to yield a cleartext.
/// The cleartext is a JSON string (that's three levels of JSON wrapping, if
/// you're keeping score: the BSO itself, BSO payload, and cleartext) with the
/// actual record payload.
///
/// An envelope combines the metadata fields from the BSO, and the cleartext
/// from the encrypted BSO payload.
#[derive(Clone, Debug, Deserialize)]
pub struct IncomingEnvelope {
    pub id: Guid,
    pub modified: ServerTimestamp,
    #[serde(default)]
    pub sortindex: Option<i32>,
    // Don't provide access to the cleartext directly. We want all callers to
    // use `IncomingEnvelope::payload`, so that we can validate the cleartext.
    cleartext: String,
}

impl IncomingEnvelope {
    /// Parses and returns the record payload from this envelope. Returns an
    /// error if the envelope's cleartext isn't valid JSON, or the payload is
    /// invalid.
    pub fn payload(&self) -> Result<Payload, Box<dyn Error>> {
        let payload: Payload = serde_json::from_str(&self.cleartext)?;
        if payload.id != self.id {
            return Err(MismatchedIdError {
                envelope: self.id.clone(),
                payload: payload.id,
            }
            .into());
        }
        Ok(payload)
    }
}

/// An envelope for an outgoing item, returned from `BridgedEngine::apply`. This
/// is similar to `IncomingEnvelope`, but omits fields that are only set by the
/// server, like `modified`.
#[derive(Clone, Debug, Serialize)]
pub struct OutgoingEnvelope {
    id: Guid,
    cleartext: String,
}

impl OutgoingEnvelope {
    /// Creates an envelope for an outgoing item. Returns an error if the
    /// payload can't be serialized to JSON.
    pub fn new(payload: Payload) -> Result<OutgoingEnvelope, Box<dyn Error>> {
        let cleartext = serde_json::to_string(&payload)?;
        Ok(OutgoingEnvelope {
            id: payload.id,
            cleartext,
        })
    }
}

/// An error returned when the ID of an incoming BSO doesn't match the ID in
/// its payload.
#[derive(Debug)]
pub struct MismatchedIdError {
    pub envelope: Guid,
    pub payload: Guid,
}

impl Error for MismatchedIdError {}

impl fmt::Display for MismatchedIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ID `{}` in envelope doesn't match `{}` in payload",
            self.envelope, self.payload
        )
    }
}
