//! The power map at run time (rp.md § Power Switching).
//!
//! A `switches[]` entry's `outputs` name each output as the box's driver
//! reports it (`GetSwitchName`), so an output is found on the box by
//! reading the names of its switches, and the refusals `set_switch`
//! makes come from the map and the registry's hold-outs alone. The MCP
//! tools in `mcp/built_in/power.rs` put the two together.
//!
//! A driver service's restart (`restart_service`) is here too: the
//! service a device's `alpaca_url` names, the request, and the wait for
//! the service's next process
//! (docs/crates/rusty-photon-service-lifecycle.md § Restart on request).

use std::time::Duration;

use ascom_alpaca::api::Switch;
use reqwest::StatusCode;
use serde::Deserialize;

use super::EntryRef;
use crate::config::{OutputConfig, SwitchConfig};

/// The path a driver service takes a restart request on.
const RESTART_PATH: &str = "/service/v1/restart";

/// The path that names the process serving a driver service.
const INSTANCE_PATH: &str = "/service/v1/instance";

/// How often `restart_service` asks the service which process answers.
pub const RESTART_POLL: Duration = Duration::from_millis(500);

/// How long `restart_service` waits for the service's next process.
///
/// It covers the service manager's restart delay (up to 10 s), the old
/// process's close deadline (10 s), and the new process's start, a serial
/// driver's eager handshake included.
pub const RESTART_DEADLINE: Duration = Duration::from_secs(60);

/// How long one read of the serving process may take, so a stalled
/// connection to a process on its way out costs one poll, not the wait.
const INSTANCE_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// What a driver service's restart routes answer with.
#[derive(Debug, Deserialize)]
struct Instance {
    instance: String,
}

/// How a restart ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restarted {
    /// Another process answers.
    Restarted,
    /// No other process answered before the deadline.
    TimedOut,
}

impl Restarted {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Restarted => "restarted",
            Self::TimedOut => "timed_out",
        }
    }
}

/// The service that serves a device: the scheme, host and port of its
/// `alpaca_url`.
///
/// # Errors
///
/// A message naming the URL when it does not parse or names no host.
pub fn service_of(alpaca_url: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(alpaca_url)
        .map_err(|e| format!("alpaca_url {alpaca_url:?} does not parse: {e}"))?;
    let origin = url.origin();
    if !origin.is_tuple() {
        return Err(format!("alpaca_url {alpaca_url:?} names no host"));
    }
    Ok(origin.ascii_serialization())
}

/// The HTTP client a restart request to `entry`'s service goes through:
/// the entry's credential and the observatory CA.
///
/// # Errors
///
/// The reason as text, when the CA file cannot be read.
pub fn restart_client(
    entry: EntryRef<'_>,
    ca_cert_path: Option<&std::path::Path>,
) -> Result<reqwest::Client, String> {
    super::alpaca::http_client(entry.address().auth, ca_cert_path).map_err(|e| e.to_string())
}

/// Ask `service` to restart, returning the id of the process that
/// restarts.
///
/// # Errors
///
/// A message naming the service when it does not answer, takes no
/// restart requests, or refuses this one.
pub async fn request_restart(client: &reqwest::Client, service: &str) -> Result<String, String> {
    let response = client
        .post(format!("{service}{RESTART_PATH}"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| format!("the service at {service} did not answer the restart request: {e}"))?;
    let status = response.status();
    if status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED {
        return Err(format!(
            "the service at {service} takes no restart requests (it answers {status})"
        ));
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "the service at {service} refused the restart request: {status} {body}"
        ));
    }
    let instance = response.json::<Instance>().await.map_err(|e| {
        format!(
            "the service at {service} took the restart request, but its answer does not read: {e}"
        )
    })?;
    Ok(instance.instance)
}

/// Wait for a process other than `old` to answer for `service`, asking
/// every `poll`, for at most `deadline`. A read that fails, as it does
/// while no process listens, counts as no answer.
pub async fn await_restart(
    client: &reqwest::Client,
    service: &str,
    old: &str,
    poll: Duration,
    deadline: Duration,
) -> Restarted {
    let url = format!("{service}{INSTANCE_PATH}");
    let wait = async {
        loop {
            tokio::time::sleep(poll).await;
            let answer = client
                .get(&url)
                .timeout(INSTANCE_READ_TIMEOUT)
                .send()
                .await
                .ok()
                .filter(|response| response.status().is_success());
            let Some(response) = answer else { continue };
            if let Ok(instance) = response.json::<Instance>().await {
                if instance.instance != old {
                    return;
                }
            }
        }
    };
    match tokio::time::timeout(deadline, wait).await {
        Ok(()) => Restarted::Restarted,
        Err(_) => Restarted::TimedOut,
    }
}

/// One switch of a power box, as `get_switch` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputReading {
    pub name: String,
    pub index: usize,
    pub can_write: bool,
    pub on: bool,
    pub value: f64,
}

/// The names of every switch the box reports, by switch id.
///
/// # Errors
///
/// The driver's error, as text, when `MaxSwitch` or a name does not
/// read.
pub async fn switch_names(device: &dyn Switch) -> Result<Vec<String>, String> {
    let count = device
        .max_switch()
        .await
        .map_err(|e| format!("could not read MaxSwitch: {e}"))?;
    let mut names = Vec::with_capacity(count);
    for index in 0..count {
        names.push(
            device
                .get_switch_name(index)
                .await
                .map_err(|e| format!("could not read the name of switch {index}: {e}"))?,
        );
    }
    Ok(names)
}

/// The switch id of the output named `output` among `names`.
///
/// # Errors
///
/// A message naming the switch and listing the names it does report.
pub fn index_of(switch_id: &str, names: &[String], output: &str) -> Result<usize, String> {
    names.iter().position(|n| n == output).ok_or_else(|| {
        format!(
            "switch '{switch_id}' reports no output named '{output}'; it reports: {}",
            names
                .iter()
                .map(|n| format!("'{n}'"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// Read switch `index` of the box: whether it is writable, on, and its
/// value.
///
/// # Errors
///
/// The driver's error, as text, for the first read that fails.
pub async fn read_output(
    device: &dyn Switch,
    index: usize,
    name: String,
) -> Result<OutputReading, String> {
    let can_write = device
        .can_write(index)
        .await
        .map_err(|e| format!("could not read CanWrite of '{name}': {e}"))?;
    let on = device
        .get_switch(index)
        .await
        .map_err(|e| format!("could not read the state of '{name}': {e}"))?;
    let value = device
        .get_switch_value(index)
        .await
        .map_err(|e| format!("could not read the value of '{name}': {e}"))?;
    Ok(OutputReading {
        name,
        index,
        can_write,
        on,
        value,
    })
}

/// The power map's entry for `output`, if `set_switch` may switch it.
///
/// The map must list the output, and to switch it off (`on` false) it
/// must not be protected and every device the map puts on it must be
/// held out. `is_held_out` answers for a device id; an id the registry
/// does not know counts as not held out.
///
/// # Errors
///
/// The refusal, in words that name the reason.
pub fn switchable<'a>(
    switch: &'a SwitchConfig,
    output: &str,
    on: bool,
    is_held_out: impl Fn(&str) -> bool,
) -> Result<&'a OutputConfig, String> {
    let Some(mapped) = switch.output(output) else {
        return Err(format!(
            "output '{output}' of switch '{}' is not in rp's power map, so rp does not know \
             what it feeds; list it under the switch's outputs to switch it through rp",
            switch.id
        ));
    };
    if on {
        return Ok(mapped);
    }
    if let Some(reason) = &mapped.protected {
        return Err(format!(
            "output '{output}' of switch '{}' is protected and is never switched off through \
             rp: {}",
            switch.id,
            reason.as_str()
        ));
    }
    let in_hands: Vec<&str> = mapped
        .devices
        .iter()
        .map(String::as_str)
        .filter(|id| !is_held_out(id))
        .collect();
    if !in_hands.is_empty() {
        return Err(format!(
            "output '{output}' of switch '{}' still feeds devices rp has not disconnected: {}; \
             disconnect each with disconnect_device before switching the output off",
            switch.id,
            in_hands.join(", ")
        ));
    }
    Ok(mapped)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::ProtectedReason;

    fn power_box() -> SwitchConfig {
        SwitchConfig {
            id: "ppba".to_string(),
            name: None,
            alpaca_url: "http://127.0.0.1:1".to_string(),
            device_number: 0,
            unique_id: None,
            auth: None,
            outputs: vec![
                OutputConfig {
                    name: "Quad 12V Output".to_string(),
                    devices: vec!["main-cam".to_string(), "mount".to_string()],
                    protected: None,
                },
                OutputConfig {
                    name: "Adjustable Output".to_string(),
                    devices: Vec::new(),
                    protected: Some(ProtectedReason::try_new("the Pi runs off it".into()).unwrap()),
                },
            ],
        }
    }

    #[test]
    fn an_unmapped_output_is_refused_either_way() {
        let config = power_box();
        for on in [true, false] {
            let err = switchable(&config, "USB Hub", on, |_| true).unwrap_err();
            assert!(err.contains("not in rp's power map"), "{err}");
        }
    }

    #[test]
    fn a_protected_output_switches_on_but_never_off() {
        let config = power_box();
        assert_eq!(
            switchable(&config, "Adjustable Output", true, |_| true)
                .unwrap()
                .name,
            "Adjustable Output"
        );
        let err = switchable(&config, "Adjustable Output", false, |_| true).unwrap_err();
        assert!(err.contains("the Pi runs off it"), "{err}");
    }

    #[test]
    fn an_output_switches_off_only_once_every_device_on_it_is_held_out() {
        let config = power_box();
        let err = switchable(&config, "Quad 12V Output", false, |id| id == "main-cam").unwrap_err();
        assert!(err.contains("mount"), "{err}");
        assert!(!err.contains("main-cam"), "{err}");
        assert_eq!(
            switchable(&config, "Quad 12V Output", false, |_| true)
                .unwrap()
                .devices,
            vec!["main-cam".to_string(), "mount".to_string()]
        );
    }

    #[test]
    fn switching_on_ignores_the_devices_still_connected() {
        let config = power_box();
        assert!(switchable(&config, "Quad 12V Output", true, |_| false).is_ok());
    }

    #[test]
    fn a_service_is_the_scheme_host_and_port_of_its_url() {
        assert_eq!(
            service_of("https://pier1.example:11112/").unwrap(),
            "https://pier1.example:11112"
        );
        assert_eq!(
            service_of("http://127.0.0.1:11121").unwrap(),
            service_of("http://127.0.0.1:11121/api").unwrap()
        );
        assert!(service_of("not a url").is_err());
        assert!(service_of("file:///tmp/x").is_err());
    }

    /// A stub driver service that answers a restart request with
    /// `restart_status`, and whose `GET /service/v1/instance` names `old`
    /// until `flip_after` reads have been served, then `new` (never, for
    /// `None`).
    async fn restart_stub(
        restart_status: axum::http::StatusCode,
        flip_after: Option<usize>,
    ) -> crate::equipment::test_support::AlpacaStub {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        use axum::routing::{get, post};
        use axum::{Json, Router};

        let reads = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                RESTART_PATH,
                post(move || async move {
                    (
                        restart_status,
                        Json(serde_json::json!({ "instance": "old" })),
                    )
                }),
            )
            .route(
                INSTANCE_PATH,
                get(move || {
                    let reads = Arc::clone(&reads);
                    async move {
                        let served = reads.fetch_add(1, Ordering::SeqCst);
                        let flipped = flip_after.is_some_and(|after| served >= after);
                        Json(serde_json::json!({ "instance": if flipped { "new" } else { "old" } }))
                    }
                }),
            );
        crate::equipment::test_support::spawn_stub(app).await
    }

    #[tokio::test]
    async fn a_restart_request_returns_the_process_that_restarts() {
        let stub = restart_stub(axum::http::StatusCode::ACCEPTED, None).await;
        let client = reqwest::Client::new();
        assert_eq!(request_restart(&client, &stub.url()).await.unwrap(), "old");
    }

    #[tokio::test]
    async fn a_service_without_the_restart_route_is_reported() {
        let stub = crate::equipment::test_support::spawn_stub(axum::Router::new()).await;
        let client = reqwest::Client::new();
        let err = request_restart(&client, &stub.url()).await.unwrap_err();
        assert!(err.contains("takes no restart requests"), "{err}");
    }

    #[tokio::test]
    async fn a_refused_restart_request_is_reported_with_its_status() {
        let stub = restart_stub(axum::http::StatusCode::UNAUTHORIZED, None).await;
        let client = reqwest::Client::new();
        let err = request_restart(&client, &stub.url()).await.unwrap_err();
        assert!(err.contains("401"), "{err}");
    }

    #[tokio::test]
    async fn the_wait_ends_when_another_process_answers() {
        let stub = restart_stub(axum::http::StatusCode::ACCEPTED, Some(2)).await;
        let client = reqwest::Client::new();
        let restarted = await_restart(
            &client,
            &stub.url(),
            "old",
            Duration::from_millis(10),
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(restarted, Restarted::Restarted);
    }

    #[tokio::test]
    async fn the_wait_times_out_while_the_old_process_answers() {
        let stub = restart_stub(axum::http::StatusCode::ACCEPTED, None).await;
        let client = reqwest::Client::new();
        let restarted = await_restart(
            &client,
            &stub.url(),
            "old",
            Duration::from_millis(10),
            Duration::from_millis(200),
        )
        .await;
        assert_eq!(restarted, Restarted::TimedOut);
        assert_eq!(restarted.as_str(), "timed_out");
    }

    #[test]
    fn an_output_is_found_by_its_exact_name() {
        let names = vec!["Quad 12V Output".to_string(), "USB Hub".to_string()];
        assert_eq!(index_of("ppba", &names, "USB Hub").unwrap(), 1);
        let err = index_of("ppba", &names, "usb hub").unwrap_err();
        assert!(err.contains("'Quad 12V Output', 'USB Hub'"), "{err}");
    }
}
