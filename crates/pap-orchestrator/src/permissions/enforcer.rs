/// Permission enforcer.
///
/// Before a capability tool call is dispatched, the enforcer:
///   1. Iterates the capability's `credentials[]` manifest declarations.
///   2. For each declared credential, retrieves the value from the store
///      (user-scoped or system-scoped as declared).
///   3. If a **required** credential is missing:
///      - For user-scoped creds: returns a `MissingCredential` error that
///        the caller surfaces to the user as an "I need your X before I can
///        do this" message.
///      - For system-scoped creds: returns an error directing the admin to
///        set it via the management API.
///   4. Builds a `serde_json::Value` object of `{ name: plaintext_value }`
///      — this is what gets serialised into `config_json` in the gRPC request.
///
/// Only credentials explicitly declared in the manifest are injected;
/// nothing else from the store is ever passed to a container.
use anyhow::Result;
use serde_json::{Map, Value};
use thiserror::Error;

use crate::capabilities::manifest::{CapabilityManifest, CredentialScope};
use crate::permissions::store::CredentialStore;

#[derive(Debug, Error)]
pub enum PermissionError {
    /// A required user-scoped credential is not yet set.
    /// The message is human-readable and should be forwarded to the user.
    #[error("I need your {credential_name} for {capability_id} before I can do this. Please set it with: /set-credential {capability_id} {credential_name} <value>")]
    MissingUserCredential {
        capability_id: String,
        credential_name: String,
    },

    /// A required system-scoped credential is not set.
    /// This is an admin/deployment error.
    #[error("System credential '{credential_name}' for capability '{capability_id}' is not configured. An administrator must set it via the credentials API.")]
    MissingSystemCredential {
        capability_id: String,
        credential_name: String,
    },

    /// A database error occurred while resolving credentials.
    #[error("Database error resolving credential '{credential_name}' for '{capability_id}': {source}")]
    DatabaseError {
        capability_id: String,
        credential_name: String,
        source: anyhow::Error,
    },
}

/// Resolve and return all credentials declared in `manifest` for `user_id`.
///
/// Returns a JSON object `{ "credential_name": "plaintext_value", … }` that
/// is safe to inject into the capability container's `config_json`.
///
/// # Errors
/// Returns `PermissionError::MissingUserCredential` or
/// `PermissionError::MissingSystemCredential` if a required credential
/// has not been set.
pub async fn resolve_credentials(
    store: &CredentialStore,
    manifest: &CapabilityManifest,
    user_id: &str,
) -> Result<Value, PermissionError> {
    let mut map = Map::new();

    for decl in &manifest.credentials {
        let value = match decl.scope {
            CredentialScope::User => {
                store
                    .get_user(user_id, &manifest.id, &decl.name)
                    .await
                    .map_err(|e| PermissionError::DatabaseError {
                        capability_id: manifest.id.clone(),
                        credential_name: decl.name.clone(),
                        source: e,
                    })?
            }
            CredentialScope::System => {
                store
                    .get_system(&manifest.id, &decl.name)
                    .await
                    .map_err(|e| PermissionError::DatabaseError {
                        capability_id: manifest.id.clone(),
                        credential_name: decl.name.clone(),
                        source: e,
                    })?
            }
        };

        match value {
            Some(v) => {
                map.insert(decl.name.clone(), Value::String(v));
            }
            None if decl.required => {
                return Err(match decl.scope {
                    CredentialScope::User => PermissionError::MissingUserCredential {
                        capability_id: manifest.id.clone(),
                        credential_name: decl.name.clone(),
                    },
                    CredentialScope::System => PermissionError::MissingSystemCredential {
                        capability_id: manifest.id.clone(),
                        credential_name: decl.name.clone(),
                    },
                });
            }
            None => {
                // Optional credential not set — omit from config_json
            }
        }
    }

    Ok(Value::Object(map))
}
