//! What `local_only` means, and the one place it is decided.
//!
//! `local_only` (`FORGE_LOCAL_ONLY`, `--local-only`) is documented as
//! "restrict to local providers". That is a confidentiality promise, so it
//! has to be enforced on what actually leaves the process:
//!
//! - generation plane: [`crate::model_from_config`] refuses to build a
//!   provider whose endpoint is not local;
//! - decision plane: `RouterStackBuilder` degrades a network decision
//!   router (which is handed the user's task text) to `static`;
//! - **every hop**: [`EgressPolicy`] travels with the HTTP client, so a
//!   redirect cannot carry a request somewhere the check never saw.
//!
//! All three consult [`endpoint_is_local`], so there is exactly one
//! definition of "local" in the workspace and `forge doctor` can report it
//! without guessing.

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use forge_config::Config;

/// Where forge draws the line for `local_only`: **this machine only**.
///
/// Local:
/// - loopback IP literals — `127.0.0.0/8`, `::1`, and the IPv4-mapped
///   disguise `::ffff:127.0.0.1`;
/// - the unspecified addresses `0.0.0.0` / `::`, which people do write in a
///   `model_base_url`; connecting to them reaches this host's loopback
///   rather than leaving it;
/// - the exact name `localhost` (case-insensitive, trailing dot allowed) —
///   see the honesty note below;
/// - a hostless `unix:`/`file:` endpoint, i.e. a socket path. No client in
///   this crate speaks one yet, so such an endpoint still fails, but it
///   fails as a transport error, which is the honest failure for it. A
///   `file://host/...` URL carries an authority and is **not** local: this
///   predicate is the crate's single answer to "is this local", and
///   answering `true` for a URL with a remote host would be wrong even
///   while no client can dial it.
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
///   whole tree for loopback, but it is an unbounded namespace forge would
///   be trusting without checking, and nothing needs it.
/// - **anything that does not parse as a URL with a host**, including a bare
///   `api.example.com/v1` with no scheme. An unparseable endpoint is exactly
///   where a wrong guess must fail safe: refusing costs an env var, allowing
///   costs confidentiality.
///
/// The host is parsed, never substring-matched: `localhost` is a hostname
/// and `http://localhost.example.com/` is not local.
///
/// **What this does not verify:** `localhost` is trusted *by name*. Forge
/// does not resolve it, so a modified `/etc/hosts`, `HOSTALIASES`, or NSS
/// resolver module can point it off-device and this predicate will not
/// notice. Accepting the name is a deliberate usability call (it is what
/// people write, and editing the hosts file needs root); anyone who needs
/// the guarantee to survive a hostile resolver should configure
/// `127.0.0.1` instead. The README says so too.
pub fn endpoint_is_local(endpoint: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(endpoint.trim()) else {
        return false;
    };
    // A socket path, but only if there really is no authority (see above).
    if matches!(url.scheme(), "unix" | "file") {
        return url.host().is_none() || url.host_str() == Some("");
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

/// How far a client forge builds is allowed to travel.
///
/// Checking the *configured* URL is not enough. reqwest's default redirect
/// policy is `redirect::Policy::limited(10)` with **no host restriction**, so
/// a `local_only`-approved loopback endpoint answering
/// `307 Location: https://elsewhere/` would make forge re-POST the prompt —
/// method and body preserved — to an authority nothing ever checked. The
/// policy therefore travels with the client and every hop is re-checked with
/// [`endpoint_is_local`], which also keeps a legitimate loopback-to-loopback
/// redirect working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EgressPolicy {
    /// `local_only` is off: reqwest's defaults apply.
    #[default]
    Unrestricted,
    /// Every request, including every redirect hop, must stay on this
    /// machine.
    LocalOnly,
}

impl EgressPolicy {
    /// The policy this configuration calls for. Every production client is
    /// built through this, so a new client cannot quietly opt out.
    pub fn from_config(config: &Config) -> Self {
        if config.local_only {
            Self::LocalOnly
        } else {
            Self::Unrestricted
        }
    }

    /// An HTTP client honouring this policy. The caller maps the build
    /// error, because "building HTTP client" is a provider error in the
    /// generation plane and a router error in the decision plane.
    pub fn client(self, timeout: Duration) -> Result<reqwest::Client, reqwest::Error> {
        let builder = reqwest::Client::builder().timeout(timeout);
        match self {
            Self::Unrestricted => builder,
            Self::LocalOnly => builder.redirect(reqwest::redirect::Policy::custom(|attempt| {
                let hop = attempt.url().to_string();
                if endpoint_is_local(&hop) {
                    attempt.follow()
                } else {
                    attempt.error(RedirectRefused { url: hop })
                }
            })),
        }
        .build()
    }
}

/// A reqwest error plus its source chain.
///
/// `reqwest::Error`'s own `Display` for a refused redirect is only "error
/// following redirect for url (<the configured one>)" — the reason, and the
/// host that was declined, live in the source. Printing just the top level
/// would report a `local_only` refusal as an unexplained transport failure
/// against the endpoint the user configured, which is the opposite of
/// honest.
pub(crate) fn error_detail(error: &reqwest::Error) -> String {
    let mut out = error.to_string();
    let mut cause = std::error::Error::source(error);
    while let Some(source) = cause {
        out.push_str(": ");
        out.push_str(&source.to_string());
        cause = source.source();
    }
    out
}

/// The error a refused redirect hop carries, so the transport failure names
/// the host forge declined to follow rather than the one it was configured
/// with.
#[derive(Debug)]
struct RedirectRefused {
    url: String,
}

impl fmt::Display for RedirectRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "local_only refused to follow a redirect to {}: not a local endpoint",
            self.url
        )
    }
}

impl std::error::Error for RedirectRefused {}

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
            // A socket-path scheme carrying a remote authority is not a
            // socket path.
            "file://evil.example.com/share/x",
            "FILE://evil.example.com/x",
            "unix://evil.example.com/sock",
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
    fn egress_policy_comes_from_the_setting() {
        let on = Config {
            local_only: true,
            ..Config::default()
        };
        assert_eq!(EgressPolicy::from_config(&on), EgressPolicy::LocalOnly);
        assert_eq!(
            EgressPolicy::from_config(&Config::default()),
            EgressPolicy::Unrestricted
        );
        // The default must be the permissive one: it mirrors
        // `local_only = false`, and every production client resolves the
        // policy from configuration rather than relying on this.
        assert_eq!(EgressPolicy::default(), EgressPolicy::Unrestricted);
    }

    #[test]
    fn both_policies_build_a_client() {
        assert!(
            EgressPolicy::LocalOnly
                .client(Duration::from_secs(1))
                .is_ok()
        );
        assert!(
            EgressPolicy::Unrestricted
                .client(Duration::from_secs(1))
                .is_ok()
        );
    }

    #[test]
    fn a_refused_redirect_names_the_host_it_declined() {
        let refused = RedirectRefused {
            url: "https://evil.example.com/v1/chat/completions".to_string(),
        };
        let message = refused.to_string();
        assert!(message.contains("https://evil.example.com"), "{message}");
        assert!(message.contains("local_only"), "{message}");
    }
}
