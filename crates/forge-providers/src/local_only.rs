//! What `local_only` means, and the one place it is decided.
//!
//! `local_only` (`FORGE_LOCAL_ONLY`, `--local-only`) is documented as
//! "restrict to local providers". That is a confidentiality promise, so it
//! has to be enforced where a configured endpoint becomes an HTTP client,
//! not only where routing decisions are made:
//!
//! - generation plane: [`crate::model_from_config`] refuses to build a
//!   provider whose endpoint is not local;
//! - decision plane: `RouterStackBuilder` degrades a network decision
//!   router (which is handed the user's task text) to `static`.
//!
//! Both consult [`endpoint_is_local`], so there is exactly one definition
//! of "local" in the workspace and `forge doctor` can report it without
//! guessing.

use std::net::IpAddr;

use forge_core::ForgeError;

/// Where forge draws the line for `local_only`: **this machine only**.
///
/// Local:
/// - loopback IP literals — `127.0.0.0/8`, `::1`, and the IPv4-mapped
///   disguise `::ffff:127.0.0.1`;
/// - the unspecified addresses `0.0.0.0` / `::`, which people do write in a
///   `model_base_url`; connecting to them reaches this host's loopback
///   rather than leaving it;
/// - the exact name `localhost` (case-insensitive, trailing dot allowed);
/// - `unix:`/`file:` endpoints — a socket path cannot leave the machine. No
///   client in this crate speaks one yet, so such an endpoint still fails,
///   but it fails as a transport error, which is the honest failure for it.
///
/// Remote — deliberately, including the judgment calls:
/// - **private-range LAN addresses** (`10/8`, `172.16/12`, `192.168/16`,
///   link-local `169.254/16`) and mDNS `*.local` names. They are off-device:
///   the request crosses a physical network to a host somebody else may
///   administer, usually over plain HTTP. Users read `local_only` as "my
///   code stays on my machine", not as a network-topology hint, so the
///   stricter reading is the honest one — and the person who really does
///   want the GPU box down the hall has an explicit way to say so: leave
///   `local_only` off.
/// - **subdomains of `localhost`** (`api.localhost`). RFC 6761 reserves the
///   whole tree for loopback, but resolvers do not all honour it, and a name
///   forge cannot verify resolves to loopback is not a guarantee.
/// - **anything that does not parse as a URL with a host**, including a bare
///   `api.example.com/v1` with no scheme. An unparseable endpoint is exactly
///   where a wrong guess must fail safe: refusing costs an env var, allowing
///   costs confidentiality.
///
/// The host is parsed, never substring-matched: `localhost` is a hostname
/// and `http://localhost.example.com/` is not local.
pub fn endpoint_is_local(endpoint: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(endpoint.trim()) else {
        return false;
    };
    if matches!(url.scheme(), "unix" | "file") {
        return true;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    match host_ip(host) {
        Some(ip) => ip_is_local(ip),
        None => host_name_is_loopback(host),
    }
}

/// `Url::host_str` keeps IPv6 literals bracketed (`[::1]`); strip the
/// brackets before asking `IpAddr` to parse.
fn host_ip(host: &str) -> Option<IpAddr> {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    bare.parse().ok()
}

fn ip_is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_unspecified(),
        // `::ffff:127.0.0.1` is loopback wearing an IPv6 costume.
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.is_loopback() || v4.is_unspecified(),
            None => v6.is_loopback() || v6.is_unspecified(),
        },
    }
}

/// Only the exact name `localhost` — see [`endpoint_is_local`] for why
/// `*.localhost` does not count.
fn host_name_is_loopback(host: &str) -> bool {
    host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
}

/// A model's resolved endpoint plus which config field produced it, so a
/// refusal can name the line to edit (same reasoning as
/// [`crate::model::credential_hint`]: a hint pointing at a setting that
/// isn't in the user's file sends them hunting).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelEndpoint {
    pub(crate) url: String,
    pub(crate) config_field: &'static str,
}

impl ModelEndpoint {
    pub(crate) fn new(url: impl Into<String>, config_field: &'static str) -> Self {
        Self {
            url: url.into(),
            config_field,
        }
    }

    /// Refuse, before any client is built, to point a provider at an
    /// endpoint that would carry the user's code off this machine.
    ///
    /// A typed config error rather than a provider error: nothing failed at
    /// the transport layer, the configuration asks for two things that
    /// cannot both be true. The message carries both honest ways forward,
    /// because either can be the real intent — the endpoint is wrong, or
    /// `local_only` is.
    pub(crate) fn ensure_local_only_allows(
        &self,
        model: &str,
        local_only: bool,
    ) -> Result<(), ForgeError> {
        if !local_only || endpoint_is_local(&self.url) {
            return Ok(());
        }
        Err(ForgeError::config(format!(
            "local_only is set, but model {model:?} would send requests to {url}, which is \
             not a local endpoint — refusing to build it rather than send your code off \
             this machine; hint: point {field} at a local server \
             (e.g. \"http://127.0.0.1:8080/v1\"), or unset local_only / FORGE_LOCAL_ONLY \
             to allow remote endpoints",
            url = self.url,
            field = self.config_field,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_endpoints_are_local() {
        for endpoint in [
            "http://127.0.0.1:8080/v1",
            "http://127.0.0.1:9",
            "http://127.9.9.9/v1",
            "https://127.0.0.1/v1",
            "http://[::1]:8080/v1",
            "http://[::ffff:127.0.0.1]:8080/v1",
            "http://localhost:8080/v1",
            "http://LocalHost:8080/v1",
            "http://localhost./v1",
            "http://0.0.0.0:8080/v1",
            "http://[::]:8080/v1",
            "  http://127.0.0.1:8080/v1  ",
            "unix:///var/run/forge/model.sock",
            "file:///tmp/model.sock",
        ] {
            assert!(endpoint_is_local(endpoint), "must be local: {endpoint}");
        }
    }

    /// The far side of the line we drew, including the LAN judgment call
    /// and the substring traps.
    #[test]
    fn off_device_endpoints_are_not_local() {
        for endpoint in [
            "https://api.anthropic.com",
            "https://api.openai.com/v1",
            // Private-range LAN: off-device, therefore refused.
            "http://192.168.1.50:8080/v1",
            "http://10.0.0.5:8080/v1",
            "http://172.16.4.2:8080/v1",
            "http://169.254.3.4:8080/v1",
            "http://gpu-box.local:8080/v1",
            // Substring traps: parsed as hosts, not searched for text.
            "http://localhost.example.com/v1",
            "http://api.localhost/v1",
            "http://notlocalhost/v1",
            "http://127.0.0.1.example.com/v1",
            // Unparseable / hostless: fail safe.
            "api.openai.com/v1",
            "",
            "not a url at all",
            "http:///v1",
        ] {
            assert!(!endpoint_is_local(endpoint), "must be remote: {endpoint}");
        }
    }

    #[test]
    fn refusal_names_the_model_the_url_and_both_ways_forward() {
        let endpoint = ModelEndpoint::new("https://api.openai.com/v1", "model_base_url");
        let err = endpoint
            .ensure_local_only_allows("gpt-5", true)
            .expect_err("remote endpoint must be refused");
        let ForgeError::Config(message) = err else {
            panic!("local_only refusals are config errors");
        };
        assert!(message.contains("gpt-5"), "{message}");
        assert!(message.contains("https://api.openai.com/v1"), "{message}");
        assert!(message.contains("model_base_url"), "{message}");
        assert!(message.contains("unset local_only"), "{message}");
        assert!(message.contains("FORGE_LOCAL_ONLY"), "{message}");
    }

    #[test]
    fn a_local_endpoint_and_a_disabled_setting_refuse_nothing() {
        let local = ModelEndpoint::new("http://127.0.0.1:8080/v1", "model_base_url");
        assert!(local.ensure_local_only_allows("m", true).is_ok());

        let remote = ModelEndpoint::new("https://api.openai.com/v1", "model_base_url");
        assert!(remote.ensure_local_only_allows("m", false).is_ok());
    }
}
