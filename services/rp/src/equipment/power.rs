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

/// How long `power_up` waits for its devices to settle when the call
/// names no `settle`.
pub const DEFAULT_SETTLE: Duration = Duration::from_secs(30);

/// The longest `settle` a `power_up` takes.
pub const MAX_SETTLE: Duration = Duration::from_mins(5);

/// The `settle` a `power_up` waits: the call's humantime duration, or
/// [`DEFAULT_SETTLE`].
///
/// # Errors
///
/// A message naming the value when it does not parse or is longer than
/// [`MAX_SETTLE`].
pub fn parse_settle(settle: Option<&str>) -> Result<Duration, String> {
    let Some(settle) = settle else {
        return Ok(DEFAULT_SETTLE);
    };
    let duration = humantime::parse_duration(settle)
        .map_err(|e| format!("settle {settle:?} is not a duration: {e}"))?;
    if duration > MAX_SETTLE {
        return Err(format!(
            "settle {settle:?} is longer than the {} a power_up waits at most",
            humantime::format_duration(MAX_SETTLE)
        ));
    }
    Ok(duration)
}

/// One output a `power_up` or `power_down` switches, with the devices
/// the power map puts on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerTarget {
    pub switch_id: String,
    pub output: String,
    pub devices: Vec<String>,
}

/// The outputs a `power_up` (`cutting` false) or a `power_down`
/// (`cutting` true) switches.
///
/// They are the ones `requested` names, as `(switch_id, output)` pairs,
/// or, when the call names none, every output the power map marks
/// `daily`, across every `switches[]` entry. An empty `requested` is no
/// output. A pair named twice counts once.
///
/// # Errors
///
/// The refusal of the whole call, before anything is switched: a switch
/// rp does not know, an output its power map does not list, and, when
/// cutting, an output it marks protected.
pub fn resolve_targets(
    switches: &[&SwitchConfig],
    requested: Option<&[(String, String)]>,
    cutting: bool,
) -> Result<Vec<PowerTarget>, String> {
    let target = |switch: &SwitchConfig, output: &OutputConfig| PowerTarget {
        switch_id: switch.id.clone(),
        output: output.name.clone(),
        devices: output.devices.clone(),
    };
    let Some(requested) = requested else {
        return Ok(switches
            .iter()
            .flat_map(|switch| {
                switch
                    .outputs
                    .iter()
                    .filter(|output| output.daily)
                    .map(|output| target(switch, output))
            })
            .collect());
    };
    let mut targets: Vec<PowerTarget> = Vec::with_capacity(requested.len());
    for (switch_id, output) in requested {
        let Some(switch) = switches.iter().find(|s| &s.id == switch_id) else {
            return Err(format!("switch not found: {switch_id}"));
        };
        let Some(mapped) = switch.output(output) else {
            return Err(format!(
                "output '{output}' of switch '{switch_id}' is not in rp's power map, so rp does \
                 not know what it feeds; list it under the switch's outputs to switch it \
                 through rp"
            ));
        };
        if cutting {
            if let Some(reason) = &mapped.protected {
                return Err(format!(
                    "output '{output}' of switch '{switch_id}' is protected and is never \
                     switched off through rp: {}",
                    reason.as_str()
                ));
            }
        }
        let next = target(switch, mapped);
        if !targets.contains(&next) {
            targets.push(next);
        }
    }
    Ok(targets)
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
                    daily: true,
                },
                OutputConfig {
                    name: "Adjustable Output".to_string(),
                    devices: Vec::new(),
                    protected: Some(ProtectedReason::try_new("the Pi runs off it".into()).unwrap()),
                    daily: false,
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
    fn without_outputs_a_call_takes_every_daily_output() {
        let config = power_box();
        let targets = resolve_targets(&[&config], None, false).unwrap();
        assert_eq!(
            targets,
            vec![PowerTarget {
                switch_id: "ppba".to_string(),
                output: "Quad 12V Output".to_string(),
                devices: vec!["main-cam".to_string(), "mount".to_string()],
            }]
        );
    }

    #[test]
    fn an_empty_list_is_no_output_not_the_daily_ones() {
        let config = power_box();
        assert_eq!(
            resolve_targets(&[&config], Some(&[]), true).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn a_named_output_is_taken_once_however_often_it_is_named() {
        let config = power_box();
        let pair = ("ppba".to_string(), "Adjustable Output".to_string());
        let targets = resolve_targets(&[&config], Some(&[pair.clone(), pair]), false).unwrap();
        assert_eq!(targets.len(), 1, "{targets:?}");
        assert_eq!(targets[0].output, "Adjustable Output");
    }

    #[test]
    fn a_call_naming_an_unknown_switch_or_unmapped_output_is_refused_whole() {
        let config = power_box();
        let err = resolve_targets(
            &[&config],
            Some(&[("upb".to_string(), "Quad 12V Output".to_string())]),
            false,
        )
        .unwrap_err();
        assert!(err.contains("switch not found: upb"), "{err}");
        let err = resolve_targets(
            &[&config],
            Some(&[
                ("ppba".to_string(), "Quad 12V Output".to_string()),
                ("ppba".to_string(), "USB Hub".to_string()),
            ]),
            false,
        )
        .unwrap_err();
        assert!(err.contains("not in rp's power map"), "{err}");
    }

    #[test]
    fn cutting_refuses_a_protected_output_but_powering_up_takes_it() {
        let config = power_box();
        let pair = [("ppba".to_string(), "Adjustable Output".to_string())];
        let err = resolve_targets(&[&config], Some(&pair), true).unwrap_err();
        assert!(err.contains("the Pi runs off it"), "{err}");
        assert_eq!(
            resolve_targets(&[&config], Some(&pair), false)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn settle_defaults_to_thirty_seconds_and_stops_at_five_minutes() {
        assert_eq!(parse_settle(None).unwrap(), Duration::from_secs(30));
        assert_eq!(
            parse_settle(Some("100ms")).unwrap(),
            Duration::from_millis(100)
        );
        assert_eq!(parse_settle(Some("5m")).unwrap(), Duration::from_mins(5));
        let err = parse_settle(Some("6m")).unwrap_err();
        assert!(err.contains("longer than"), "{err}");
        let err = parse_settle(Some("soon")).unwrap_err();
        assert!(err.contains("not a duration"), "{err}");
    }

    #[test]
    fn an_output_is_found_by_its_exact_name() {
        let names = vec!["Quad 12V Output".to_string(), "USB Hub".to_string()];
        assert_eq!(index_of("ppba", &names, "USB Hub").unwrap(), 1);
        let err = index_of("ppba", &names, "usb hub").unwrap_err();
        assert!(err.contains("'Quad 12V Output', 'USB Hub'"), "{err}");
    }
}
