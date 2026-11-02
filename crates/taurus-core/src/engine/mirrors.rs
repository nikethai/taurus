//! Checking a new job's mirrors: each must serve the primary's file.

use crate::http::probe::validator_change;
use crate::http::{Auth, Probe, resolve_with};
use crate::system::marks::loggable;

use super::jobs::{describe_size, fixed_url};

/// The mirrors that serve the primary's file, each as the link to request
/// (a confirm form's, if one was followed). A mirror that fails to resolve,
/// serves another size, or whose validators say it serves another version
/// (the rule segment responses follow) is dropped. `auth` signs in where
/// it applies: a mirror on the link's own origin.
pub(super) async fn vet_mirrors(
    client: &reqwest::Client,
    primary: &Probe,
    mirrors: &[String],
    referer: Option<&str>,
    auth: Option<&Auth>,
) -> Vec<String> {
    let mut kept = Vec::new();
    for mirror in mirrors {
        let why = match resolve_with(client, &fixed_url(mirror), referer, auth).await {
            Err(e) => e.to_string(),
            Ok(probe) if probe.size != primary.size => format!(
                "size {} differs from {}",
                describe_size(probe.size),
                describe_size(primary.size)
            ),
            Ok(probe) => match mirror_change(primary, &probe) {
                Some(change) => change,
                None => {
                    kept.push(probe.requested_url.to_string());
                    continue;
                }
            },
        };
        tracing::info!(mirror = loggable(mirror), "dropping the mirror: {why}");
    }
    kept
}

/// How the mirror's validators differ from the primary's, as for a 206.
fn mirror_change(primary: &Probe, mirror: &Probe) -> Option<String> {
    validator_change(
        (primary.etag.as_deref(), primary.last_modified.as_deref()),
        (mirror.etag.as_deref(), mirror.last_modified.as_deref()),
    )
    .map(|what| format!("{what} (primary to mirror)"))
}
