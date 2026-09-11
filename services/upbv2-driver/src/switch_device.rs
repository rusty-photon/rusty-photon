//! `UPBv2` Switch device implementation.
//!
//! Implements the ASCOM Alpaca `Device` + `Switch` traits over the 39-switch
//! table in `docs/services/upbv2-driver.md`. Connection state is the device's
//! `Session<Upbv2Codec>` slot — when it's `Some`, we hold a live handle to the
//! shared transport; when it's `None`, we don't. There is no second
//! "requested" bool that could diverge from the transport's refcount.
//!
//! Two things separate this device from `ppba-driver`'s:
//!
//! * **Reads never touch the wire.** Every value comes from the cache the
//!   read-only handshake seeds and the poll loop refreshes; a cache miss is
//!   `NOT_CONNECTED`, per the design doc's error table.
//! * **The auto-dew gate is per channel.** `PA` field 20 carries a channel
//!   mask, so channel A can be read-only while B and C stay writable. The
//!   driver never writes `PD:` — turning auto-dew off is done in the Pegasus
//!   Astro software.

use std::sync::Arc;

use ascom_alpaca::api::{Device, Switch};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use async_trait::async_trait;
use rusty_photon_shared_transport::Session;
use tokio::sync::RwLock;
use tracing::debug;

use crate::codec::Upbv2Codec;
use crate::config::SwitchConfig;
use crate::config_actions::Upbv2Driver;
use crate::error::{Result, Upbv2Error};
use crate::manager::Upbv2Manager;
use crate::protocol::{DewChannel, PwmDuty, Upbv2Command, VariableVolts, OUTPUT_COUNT};
use crate::switches::{SwitchId, MAX_SWITCH};
use rusty_photon_driver::ConfigActionCtx;

/// Guard macro that returns `NOT_CONNECTED` if the device is not connected.
macro_rules! ensure_connected {
    ($self:ident) => {
        if !$self.connected().await.is_ok_and(|c| c) {
            debug!("Switch device not connected");
            return Err(ASCOMError::NOT_CONNECTED);
        }
    };
}

/// Resolve a switch id, mapping an unknown one to `INVALID_VALUE`.
///
/// Written once as a macro because the `?` has to return from the *caller*,
/// and every metadata accessor in the `Switch` impl needs the same two lines.
macro_rules! switch_id_or_invalid {
    ($id:expr) => {
        SwitchId::from_id($id).ok_or_else(|| {
            ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Invalid switch ID: {}", $id),
            )
        })?
    };
}

/// ASCOM switch values are `f64`; boolean device state is 1.0 / 0.0.
const fn bool_value(state: bool) -> f64 {
    if state {
        1.0
    } else {
        0.0
    }
}

/// Read one element of a fixed-size per-channel array out of the cache.
///
/// `clippy::indexing_slicing` is denied workspace-wide — a panic at 2 a.m.
/// ends the night (tenet 2) — so the bounds check is explicit even though
/// every index here comes from a constructor-validated channel type
/// ([`OutputId`], [`DewChannel`], [`UsbPortId`]) and cannot be out of range.
///
/// [`OutputId`]: crate::protocol::OutputId
/// [`UsbPortId`]: crate::protocol::UsbPortId
fn element<T: Copy>(array: &[T], index: usize, id: usize) -> Result<T> {
    array
        .get(index)
        .copied()
        .ok_or(Upbv2Error::InvalidSwitchId(id))
}

/// `UPBv2` Switch device for ASCOM Alpaca.
#[derive(derive_more::Debug)]
pub struct Upbv2SwitchDevice {
    config: SwitchConfig,
    /// `Some` between successful connect and explicit disconnect. The
    /// session existing is the truth — no second-source bool to desync.
    #[debug(skip)]
    session: Arc<RwLock<Option<Session<Upbv2Codec>>>>,
    #[debug(skip)]
    manager: Arc<Upbv2Manager>,
    /// Shared (cloned) config-action context; `Some` on the normal path through
    /// `ServerBuilder`, `None` for focused unit-test devices.
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<Upbv2Driver>>,
}

impl Upbv2SwitchDevice {
    #[must_use]
    pub fn new(config: SwitchConfig, manager: Arc<Upbv2Manager>) -> Self {
        Self {
            config,
            session: Arc::new(RwLock::new(None)),
            manager,
            config_ctx: None,
        }
    }

    /// Attach the shared config-action context, enabling `config.get` /
    /// `config.apply` / `config.schema` on this device.
    #[must_use]
    pub fn with_config_actions(mut self, ctx: ConfigActionCtx<Upbv2Driver>) -> Self {
        self.config_ctx = Some(ctx);
        self
    }

    /// Serve one switch value from the cache.
    ///
    /// Nothing here reaches the wire: `PA`, `PC` and `PS` are all seeded by
    /// the read-only handshake and refreshed by the poll loop. A missing cache
    /// slot therefore means "no successful poll yet", which the design doc's
    /// error table maps to `NOT_CONNECTED`.
    async fn get_switch_value_internal(&self, id: usize) -> Result<f64> {
        let switch_id = SwitchId::from_id(id).ok_or(Upbv2Error::InvalidSwitchId(id))?;
        let cached = self.manager.get_cached_state().await;
        // `Option<&T>` is `Copy`, so each arm takes the slot it needs without
        // re-borrowing the snapshot or repeating the `as_ref()`.
        let status = cached.status.as_ref();
        let power = cached.power.as_ref();
        let boot_state = cached.boot_state.as_ref();

        match switch_id {
            // ---- Controllable, read back from `PA` ----
            SwitchId::Output(port) => {
                let status = status.ok_or(Upbv2Error::NotConnected)?;
                Ok(bool_value(element(&status.outputs, port.index(), id)?))
            }
            SwitchId::DewHeater(channel) => {
                let status = status.ok_or(Upbv2Error::NotConnected)?;
                Ok(f64::from(element(&status.dew_duty, channel.index(), id)?))
            }
            // `PA` has no variable-output field; `PS` is the one place the
            // device reports the setpoint.
            SwitchId::VariableVoltage => {
                let boot_state = boot_state.ok_or(Upbv2Error::NotConnected)?;
                Ok(f64::from(boot_state.variable_volts))
            }
            // Unlike the PPBA, `PA` reports USB state directly — no shadow
            // state to keep in step with the hardware.
            SwitchId::UsbPort(port) => {
                let status = status.ok_or(Upbv2Error::NotConnected)?;
                Ok(bool_value(element(&status.usb_ports, port.index(), id)?))
            }

            // ---- Read-only sensors ----
            SwitchId::InputVoltage => Ok(status.ok_or(Upbv2Error::NotConnected)?.voltage),
            SwitchId::TotalCurrent => Ok(status.ok_or(Upbv2Error::NotConnected)?.current),
            SwitchId::PowerDraw => Ok(f64::from(status.ok_or(Upbv2Error::NotConnected)?.power)),
            SwitchId::Temperature => Ok(status.ok_or(Upbv2Error::NotConnected)?.temperature),
            SwitchId::Humidity => Ok(status.ok_or(Upbv2Error::NotConnected)?.humidity),
            SwitchId::Dewpoint => Ok(status.ok_or(Upbv2Error::NotConnected)?.dewpoint),

            // ---- Read-only per-channel telemetry ----
            // The parser already divided out the sense resistors, so the
            // switch surface only ever sees Amps.
            SwitchId::OutputCurrent(port) => {
                let status = status.ok_or(Upbv2Error::NotConnected)?;
                element(&status.output_current, port.index(), id)
            }
            SwitchId::DewCurrent(channel) => {
                let status = status.ok_or(Upbv2Error::NotConnected)?;
                element(&status.dew_current, channel.index(), id)
            }
            // One 7-flag `PA` field covers both families: outputs 1-4 first,
            // then dew A-C. Exposed per channel because when a rail trips at
            // 2 a.m. the useful fact is *which* one.
            SwitchId::OutputOvercurrent(port) => {
                let status = status.ok_or(Upbv2Error::NotConnected)?;
                Ok(bool_value(element(&status.overcurrent, port.index(), id)?))
            }
            SwitchId::DewOvercurrent(channel) => {
                let status = status.ok_or(Upbv2Error::NotConnected)?;
                let index = OUTPUT_COUNT.saturating_add(channel.index());
                Ok(bool_value(element(&status.overcurrent, index, id)?))
            }
            // Surfaced so a client can *explain* a false `CanWrite` on
            // switches 4-6 rather than just observe it.
            SwitchId::AutoDewChannels => {
                let status = status.ok_or(Upbv2Error::NotConnected)?;
                Ok(f64::from(status.auto_dew.raw()))
            }

            // ---- Read-only power counters, from `PC` ----
            SwitchId::AverageCurrent => Ok(power.ok_or(Upbv2Error::NotConnected)?.average_amps),
            SwitchId::AmpHours => Ok(power.ok_or(Upbv2Error::NotConnected)?.amp_hours),
            SwitchId::WattHours => Ok(power.ok_or(Upbv2Error::NotConnected)?.watt_hours),
            SwitchId::Uptime => Ok(power.ok_or(Upbv2Error::NotConnected)?.uptime_hours()),
        }
    }

    async fn set_switch_value_internal(&self, id: usize, value: f64) -> Result<()> {
        let switch_id = SwitchId::from_id(id).ok_or(Upbv2Error::InvalidSwitchId(id))?;
        let info = switch_id.info();

        if !info.can_write {
            return Err(Upbv2Error::SwitchNotWritable(id));
        }

        let guard = self.session.read().await;
        let session = guard.as_ref().ok_or(Upbv2Error::NotConnected)?;

        // Dew heaters: re-read auto-dew off the *device*, not the cache. The
        // operator can turn auto-dew on from the Pegasus software between two
        // polls, and losing a heater write to a stale mask is exactly the
        // failure the per-channel gate exists to prevent.
        if let SwitchId::DewHeater(channel) = switch_id {
            self.manager.refresh_status(session).await?;
            let cached = self.manager.get_cached_state().await;
            if let Some(status) = &cached.status {
                if status.auto_dew.controls(channel) {
                    debug!(
                        id,
                        channel = channel.label(),
                        mask = status.auto_dew.raw(),
                        "rejecting dew write: channel is under auto-dew control"
                    );
                    return Err(Upbv2Error::AutoDewControlled {
                        switch_id: id,
                        channel: channel.label(),
                    });
                }
            }
        }

        // `!is_finite()` is checked explicitly: NaN compares false against
        // both bounds, so it would otherwise slip through and silently
        // actuate (a boolean switch reads NaN as off, PWM clamps it to 0).
        if !value.is_finite() || value < info.min_value || value > info.max_value {
            return Err(Upbv2Error::InvalidValue(format!(
                "Value {} out of range [{}, {}] for switch {}",
                value, info.min_value, info.max_value, info.name
            )));
        }

        // The boolean switches are validated to [0, 1] just above, so `> 0.0`
        // is "any non-zero value means on" without an equality comparison
        // between floats.
        let command = match switch_id {
            SwitchId::Output(port) => Upbv2Command::SetOutput(port, value > 0.0),
            SwitchId::DewHeater(channel) => Upbv2Command::SetDew(channel, PwmDuty::from(value)),
            // Rejected rather than clamped: this write lands in the device's
            // EEPROM, so a client asking for 24 V has a bug worth reporting.
            SwitchId::VariableVoltage => {
                Upbv2Command::SetVariableVoltage(VariableVolts::from_switch_value(value)?)
            }
            SwitchId::UsbPort(port) => Upbv2Command::SetUsb(port, value > 0.0),
            _ => return Err(Upbv2Error::SwitchNotWritable(id)),
        };

        debug!(id, value, "sending switch write");
        self.manager.send_command(session, command).await?;

        // Refresh so the cached view reflects the new device state. Which
        // reply carries it depends on the switch: `PA` has no
        // variable-voltage field, so refreshing status there would leave
        // switch 7 reporting its pre-write setpoint until something else
        // re-read `PS`.
        if matches!(switch_id, SwitchId::VariableVoltage) {
            self.manager.refresh_boot_state(session).await?;
        } else {
            self.manager.refresh_status(session).await?;
        }
        drop(guard);
        Ok(())
    }

    /// Whether `channel`'s dew heater is writable right now.
    ///
    /// Per channel, not per device: the mask can have the device driving A
    /// while the operator still owns B and C. Served from the cached mask,
    /// refreshing once under the device's session if the cache has not been
    /// populated yet.
    async fn dew_heater_writable(&self, channel: DewChannel) -> ASCOMResult<bool> {
        let cached = self.manager.get_cached_state().await;
        if let Some(status) = &cached.status {
            return Ok(!status.auto_dew.controls(channel));
        }

        let guard = self.session.read().await;
        let session = guard
            .as_ref()
            .ok_or_else(|| ASCOMError::new(ASCOMErrorCode::NOT_CONNECTED, "not connected"))?;
        self.manager.refresh_status(session).await?;
        drop(guard);

        let cached = self.manager.get_cached_state().await;
        Ok(cached
            .status
            .as_ref()
            .is_none_or(|status| !status.auto_dew.controls(channel)))
    }
}

#[async_trait]
impl Device for Upbv2SwitchDevice {
    fn static_name(&self) -> &str {
        &self.config.name
    }

    fn unique_id(&self) -> &str {
        &self.config.unique_id
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok(self.config.description.clone())
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(self.session.read().await.is_some() && self.manager.is_available())
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the write lock deliberately spans the whole check-and-modify so two concurrent connects cannot both observe an empty slot and double-acquire"
    )]
    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        // The write lock spans the whole check-and-modify so two concurrent
        // `Connected=true` requests can't both observe `None` and both call
        // `acquire()`. With the session slot replacing a separate `requested`
        // bool, the flag and the resource are the same value — there is no
        // second source to desync.
        //
        // Connecting actuates nothing: the handshake behind `acquire()` is
        // `P#` / `PV` / `PA` / `PC` / `PS`, every one of them a read (tenet 3).
        let mut slot = self.session.write().await;
        match (connected, slot.is_some()) {
            (true, false) => {
                // `?` does SessionError → Upbv2Error via the manual
                // .map_err, then Upbv2Error → ASCOMError via the From impl
                // generated in error.rs.
                let session = self
                    .manager
                    .transport()
                    .acquire()
                    .await
                    .map_err(Upbv2Error::from)?;
                *slot = Some(session);
                debug!("Switch device connected");
            }
            (false, true) => {
                if let Some(session) = slot.take() {
                    // `Session::close` returns Result<_, TransportError>;
                    // `From<TransportError> for Upbv2Error` handles the
                    // conversion and the `From<Upbv2Error> for ASCOMError`
                    // impl does the second hop on `?`.
                    session.close().await.map_err(Upbv2Error::from)?;
                }
                debug!("Switch device disconnected");
            }
            _ => {}
        }
        Ok(())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("UPBv2 Driver - Switch interface for Pegasus Astro Ultimate Powerbox v2".to_string())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_string())
    }

    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        Ok(rusty_photon_driver::supported_actions(&self.config_ctx))
    }

    async fn action(&self, action: String, parameters: String) -> ASCOMResult<String> {
        rusty_photon_driver::dispatch::<Upbv2Driver>(&self.config_ctx, action, parameters).await
    }
}

#[async_trait]
impl Switch for Upbv2SwitchDevice {
    async fn max_switch(&self) -> ASCOMResult<usize> {
        Ok(MAX_SWITCH)
    }

    async fn can_write(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);

        let switch_id = switch_id_or_invalid!(id);

        if let SwitchId::DewHeater(channel) = switch_id {
            return self.dew_heater_writable(channel).await;
        }

        Ok(switch_id.info().can_write)
    }

    async fn get_switch(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);

        let value = self.get_switch_value_internal(id).await?;

        let switch_id = switch_id_or_invalid!(id);
        Ok(value > switch_id.info().min_value)
    }

    async fn set_switch(&self, id: usize, state: bool) -> ASCOMResult<()> {
        ensure_connected!(self);

        let switch_id = switch_id_or_invalid!(id);
        let info = switch_id.info();

        let value = if state {
            info.max_value
        } else {
            info.min_value
        };

        self.set_switch_value_internal(id, value).await?;
        Ok(())
    }

    async fn get_switch_description(&self, id: usize) -> ASCOMResult<String> {
        ensure_connected!(self);
        let switch_id = switch_id_or_invalid!(id);
        Ok(switch_id.info().description.to_string())
    }

    /// The operator's label for this switch's port where the config carries
    /// one, otherwise the built-in name. `GetSwitchDescription` is deliberately
    /// left alone: it names the physical port, so a labelled switch is still
    /// identifiable as the connector it is.
    async fn get_switch_name(&self, id: usize) -> ASCOMResult<String> {
        ensure_connected!(self);
        let switch_id = switch_id_or_invalid!(id);
        Ok(switch_id.effective_name(&self.config.labels))
    }

    async fn set_switch_name(&self, _id: usize, _name: String) -> ASCOMResult<()> {
        Err(ASCOMError::new(
            ASCOMErrorCode::NOT_IMPLEMENTED,
            "Setting switch names is not supported",
        ))
    }

    async fn get_switch_value(&self, id: usize) -> ASCOMResult<f64> {
        ensure_connected!(self);

        Ok(self.get_switch_value_internal(id).await?)
    }

    async fn set_switch_value(&self, id: usize, value: f64) -> ASCOMResult<()> {
        ensure_connected!(self);

        self.set_switch_value_internal(id, value).await?;
        Ok(())
    }

    async fn min_switch_value(&self, id: usize) -> ASCOMResult<f64> {
        ensure_connected!(self);
        let switch_id = switch_id_or_invalid!(id);
        Ok(switch_id.info().min_value)
    }

    async fn max_switch_value(&self, id: usize) -> ASCOMResult<f64> {
        ensure_connected!(self);
        let switch_id = switch_id_or_invalid!(id);
        Ok(switch_id.info().max_value)
    }

    async fn switch_step(&self, id: usize) -> ASCOMResult<f64> {
        ensure_connected!(self);
        let switch_id = switch_id_or_invalid!(id);
        Ok(switch_id.info().step)
    }

    async fn can_async(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);
        if id >= MAX_SWITCH {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Invalid switch ID: {id}"),
            ));
        }
        Ok(false)
    }

    async fn state_change_complete(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);
        if id >= MAX_SWITCH {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Invalid switch ID: {id}"),
            ));
        }
        Ok(true)
    }

    async fn cancel_async(&self, id: usize) -> ASCOMResult<()> {
        ensure_connected!(self);
        if id >= MAX_SWITCH {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Invalid switch ID: {id}"),
            ));
        }
        Ok(())
    }

    async fn set_async(&self, id: usize, state: bool) -> ASCOMResult<()> {
        ensure_connected!(self);
        if id >= MAX_SWITCH {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Invalid switch ID: {id}"),
            ));
        }
        self.set_switch(id, state).await
    }

    async fn set_async_value(&self, id: usize, value: f64) -> ASCOMResult<()> {
        ensure_connected!(self);
        if id >= MAX_SWITCH {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Invalid switch ID: {id}"),
            ));
        }
        self.set_switch_value(id, value).await
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    //! Unit tests cover the switch table's read mapping, the per-channel
    //! auto-dew gate, write validation, and happy-path
    //! connect/read/write/disconnect. Race / refcount / rollback invariants
    //! are tested once in `rusty-photon-shared-transport`'s `tests/race.rs`
    //! and `tests/rollback.rs`; they are not duplicated per service.
    //!
    //! The simulator below is local to this file per testing.md §6.5. It is
    //! deliberately *not* `crate::mock`'s: these tests need the auto-dew mask
    //! set to an arbitrary value before connect, which no command in
    //! [`Upbv2Command`] can do — the driver never sends `PD:`.

    use super::*;
    use crate::config::Config;
    use crate::protocol::{DEW_COUNT, OVERCURRENT_COUNT, USB_COUNT};
    use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};
    use std::collections::VecDeque;
    use tokio::sync::Mutex;

    /// `PC` reply: average amps, amp-hours, watt-hours, uptime in ms. The
    /// vendor table gives it no command echo, so it goes on the wire bare.
    const POWER_RESPONSE: &str = "2.5:10.5:126.0:3600000";
    /// Uptime the `PC` fixture above encodes, in hours.
    const UPTIME_HOURS: f64 = 1.0;

    /// Auto-dew mask that puts channel B — and only channel B — under device
    /// control, per the design doc's mask table.
    const MASK_B_ONLY: u8 = 3;

    // ------------------------------------------------------------------
    // In-memory UPBv2 simulator
    // ------------------------------------------------------------------

    /// Render a run of flags as the `0`/`1` characters `PA` uses.
    fn flags(bits: &[bool]) -> String {
        bits.iter().map(|b| if *b { '1' } else { '0' }).collect()
    }

    #[derive(Debug, Clone)]
    struct MockDeviceState {
        outputs: [bool; OUTPUT_COUNT],
        usb_ports: [bool; USB_COUNT],
        dew_duty: [u8; DEW_COUNT],
        overcurrent: [bool; OVERCURRENT_COUNT],
        variable_volts: u8,
        auto_dew: u8,
    }

    impl Default for MockDeviceState {
        fn default() -> Self {
            Self {
                // Distinct per channel so a mis-indexed read fails loudly
                // instead of matching its neighbour.
                outputs: [true, true, false, true],
                usb_ports: [true, true, true, true, false, true],
                dew_duty: [10, 20, 30],
                // Output 3 and dew C tripped.
                overcurrent: [false, false, true, false, false, false, true],
                variable_volts: 5,
                // Auto-dew off by default, matching the ConformU precondition.
                auto_dew: 0,
            }
        }
    }

    impl MockDeviceState {
        /// A 21-token `PA` frame. The raw sense counts are chosen so the
        /// parser's divisors (480, and 700 for dew C) yield round Amps:
        /// outputs 1.0 / 2.0 / 0.0 / 0.5 A, dew A/B/C 1.0 / 0.5 / 1.0 A.
        fn status_response(&self) -> String {
            format!(
                "UPB2:12.2:1.5:18:23.2:59:14.7:{ports}:{usb}:{d0}:{d1}:{d2}:\
                 480:960:0:240:480:240:700:{oc}:{mask}",
                ports = flags(&self.outputs),
                usb = flags(&self.usb_ports),
                d0 = self.dew_duty[0],
                d1 = self.dew_duty[1],
                d2 = self.dew_duty[2],
                oc = flags(&self.overcurrent),
                mask = self.auto_dew,
            )
        }

        fn boot_state_response(&self) -> String {
            format!("PS:1100:{}", self.variable_volts)
        }

        fn respond(&mut self, command: &str) -> String {
            match command {
                "P#" => return "UPB2_OK".to_string(),
                "PV" => return "2.4".to_string(),
                "PA" => return self.status_response(),
                "PC" => return POWER_RESPONSE.to_string(),
                "PS" => return self.boot_state_response(),
                _ => {}
            }

            let Some((head, value)) = command.split_once(':') else {
                return "ERR".to_string();
            };
            let mut chars = head.chars();
            let (Some(letter), Some(digit), None) = (chars.next(), chars.next(), chars.next())
            else {
                return "ERR".to_string();
            };
            let Some(n) = digit.to_digit(10) else {
                return "ERR".to_string();
            };

            match (letter, n) {
                ('P', 1..=4) => {
                    let on = value == "1";
                    self.outputs[(n - 1) as usize] = on;
                    format!("P{n}:{}", u8::from(on))
                }
                ('P', 5..=7) => {
                    let Ok(duty) = value.parse::<u8>() else {
                        return "ERR".to_string();
                    };
                    self.dew_duty[(n - 5) as usize] = duty;
                    format!("P{n}:{duty}")
                }
                ('P', 8) => {
                    let Ok(volts) = value.parse::<u8>() else {
                        return "ERR".to_string();
                    };
                    self.variable_volts = volts;
                    format!("P8:{volts}")
                }
                ('U', 1..=6) => {
                    let on = value == "1";
                    self.usb_ports[(n - 1) as usize] = on;
                    format!("U{n}:{}", u8::from(on))
                }
                _ => "ERR".to_string(),
            }
        }
    }

    #[derive(Debug, Default)]
    struct MockState {
        response_queue: VecDeque<Vec<u8>>,
        device_state: MockDeviceState,
    }

    impl MockState {
        fn process_command(&mut self, command_bytes: &[u8]) {
            let command = std::str::from_utf8(command_bytes)
                .unwrap_or_default()
                .trim()
                .to_string();
            let mut frame = self.device_state.respond(&command).into_bytes();
            frame.push(b'\n');
            self.response_queue.push_back(frame);
        }
    }

    struct MockFrameTransport {
        state: Arc<Mutex<MockState>>,
    }

    #[async_trait]
    impl FrameTransport for MockFrameTransport {
        async fn send_frame(&mut self, bytes: &[u8]) -> std::result::Result<(), TransportError> {
            self.state.lock().await.process_command(bytes);
            Ok(())
        }

        async fn recv_frame(
            &mut self,
            buf: &mut Vec<u8>,
        ) -> std::result::Result<(), TransportError> {
            let frame = self
                .state
                .lock()
                .await
                .response_queue
                .pop_front()
                .ok_or(TransportError::Eof)?;
            buf.clear();
            buf.extend_from_slice(&frame);
            Ok(())
        }
    }

    /// Factory over a persistent simulated device, so a reconnect keeps the
    /// settings a prior write made — as real hardware does.
    #[derive(Clone, Default)]
    struct MockUpbv2Factory {
        state: Arc<Mutex<MockState>>,
    }

    impl MockUpbv2Factory {
        /// A device whose auto-dew mask is already set, the way an operator
        /// leaves it in the Pegasus software. No driver command can do this —
        /// `PD:` is deliberately absent from [`Upbv2Command`].
        fn with_auto_dew(mask: u8) -> Self {
            Self {
                state: Arc::new(Mutex::new(MockState {
                    response_queue: VecDeque::new(),
                    device_state: MockDeviceState {
                        auto_dew: mask,
                        ..MockDeviceState::default()
                    },
                })),
            }
        }

        /// The setpoint the *device* currently holds, independent of the
        /// driver's cache — the only way to prove a rejected write never
        /// reached the wire.
        async fn device_volts(&self) -> u8 {
            self.state.lock().await.device_state.variable_volts
        }
    }

    #[async_trait]
    impl TransportFactory for MockUpbv2Factory {
        async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
            Ok(Box::new(MockFrameTransport {
                state: Arc::clone(&self.state),
            }))
        }
    }

    /// Factory whose `open()` always fails, to exercise the
    /// `set_connected(true)` acquire-failure mapping into ASCOM errors.
    struct FailingUpbv2Factory;

    #[async_trait]
    impl TransportFactory for FailingUpbv2Factory {
        async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
            Err(TransportError::Open(std::io::Error::other(
                "mock factory error",
            )))
        }
    }

    // ------------------------------------------------------------------
    // Fixtures
    // ------------------------------------------------------------------

    fn make_device_with(factory: Arc<MockUpbv2Factory>) -> Upbv2SwitchDevice {
        let config = Config::default();
        let manager = Upbv2Manager::new(&config, factory);
        Upbv2SwitchDevice::new(config.switch, manager)
    }

    fn make_device() -> Upbv2SwitchDevice {
        make_device_with(Arc::new(MockUpbv2Factory::default()))
    }

    async fn connected_device() -> Upbv2SwitchDevice {
        let device = make_device();
        device.set_connected(true).await.unwrap();
        device
    }

    /// A connected device whose hardware already has `mask` under auto-dew
    /// control, plus the factory so a test can inspect the device directly.
    async fn connected_device_with_auto_dew(
        mask: u8,
    ) -> (Upbv2SwitchDevice, Arc<MockUpbv2Factory>) {
        let factory = Arc::new(MockUpbv2Factory::with_auto_dew(mask));
        let device = make_device_with(Arc::clone(&factory));
        device.set_connected(true).await.unwrap();
        (device, factory)
    }

    #[track_caller]
    fn assert_value(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    // ------------------------------------------------------------------
    // Connection lifecycle
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn starts_disconnected() {
        let device = make_device();
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn connect_then_disconnect_round_trip() {
        let device = make_device();
        device.set_connected(true).await.unwrap();
        assert!(device.connected().await.unwrap());
        device.set_connected(false).await.unwrap();
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn set_connected_is_idempotent() {
        let device = make_device();
        device.set_connected(true).await.unwrap();
        device.set_connected(true).await.unwrap();
        assert!(device.connected().await.unwrap());
        device.set_connected(false).await.unwrap();
        device.set_connected(false).await.unwrap();
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn connect_leaves_every_output_untouched() {
        // Tenet 3: the handshake is read-only, so the outputs the device
        // booted with are still the outputs after connect.
        let device = connected_device().await;
        for (id, expected) in [(0, 1.0), (1, 1.0), (2, 0.0), (3, 1.0)] {
            assert_value(device.get_switch_value(id).await.unwrap(), expected);
        }
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn operations_fail_when_not_connected() {
        let device = make_device();
        assert_eq!(
            device.get_switch(0).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.get_switch_value(0).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.set_switch(0, true).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.set_switch_value(0, 1.0).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.can_write(0).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.get_switch_name(0).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
    }

    #[tokio::test]
    async fn set_connected_acquire_failure_maps_to_invalid_operation() {
        // open() returns TransportError::Open, which propagates as a
        // connection failure and falls to the error enum's catch-all
        // classification, INVALID_OPERATION.
        let factory = Arc::new(FailingUpbv2Factory);
        let config = Config::default();
        let manager = Upbv2Manager::new(&config, factory);
        let device = Upbv2SwitchDevice::new(config.switch, manager);

        let err = device.set_connected(true).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert!(
            err.message.contains("mock factory error"),
            "expected message to carry the underlying io error, got: {}",
            err.message
        );
        // No session got stored on failure — the device stays disconnected.
        assert!(!device.connected().await.unwrap());
    }

    // ------------------------------------------------------------------
    // Read paths — every id in the table
    // ------------------------------------------------------------------

    /// A connected device whose config labels `12V Output 1` as `QHY600`.
    async fn connected_device_with_a_labelled_output() -> Upbv2SwitchDevice {
        let mut config = Config::default();
        config.switch.labels = serde_json::from_str(r#"{"12V Output 1": "QHY600"}"#).unwrap();
        let manager = Upbv2Manager::new(&config, Arc::new(MockUpbv2Factory::default()));
        let device = Upbv2SwitchDevice::new(config.switch, manager);
        device.set_connected(true).await.unwrap();
        device
    }

    #[tokio::test]
    async fn a_labelled_output_reports_the_label_on_itself_and_its_telemetry() {
        let device = connected_device_with_a_labelled_output().await;
        assert_eq!(device.get_switch_name(0).await.unwrap(), "QHY600");
        assert_eq!(device.get_switch_name(20).await.unwrap(), "QHY600 Current");
        assert_eq!(
            device.get_switch_name(27).await.unwrap(),
            "QHY600 Overcurrent"
        );
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn a_label_leaves_the_description_naming_the_physical_port() {
        let device = connected_device_with_a_labelled_output().await;
        assert_eq!(
            device.get_switch_description(0).await.unwrap(),
            "Switches the 12V output on port 1"
        );
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn an_unlabelled_switch_still_reports_its_built_in_name() {
        let device = connected_device_with_a_labelled_output().await;
        assert_eq!(device.get_switch_name(1).await.unwrap(), "12V Output 2");
        assert_eq!(device.get_switch_name(17).await.unwrap(), "Temperature");
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn every_switch_id_reads_a_value() {
        // Guards the whole table at once: any id the handshake fails to seed
        // a cache slot for (PA, PC *and* PS) surfaces here as NOT_CONNECTED.
        let device = connected_device().await;
        for id in 0..MAX_SWITCH {
            device.get_switch_value(id).await.unwrap();
        }
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn every_switch_id_has_name_and_description() {
        let device = connected_device().await;
        let mut names = Vec::with_capacity(MAX_SWITCH);
        for id in 0..MAX_SWITCH {
            let name = device.get_switch_name(id).await.unwrap();
            assert!(!name.is_empty(), "switch {id} has no name");
            assert!(
                !device.get_switch_description(id).await.unwrap().is_empty(),
                "switch {id} has no description"
            );
            names.push(name);
        }
        names.sort_unstable();
        let unique = names.len();
        names.dedup();
        assert_eq!(names.len(), unique, "switch names must be unique");
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn reads_map_to_their_wire_source() {
        let device = connected_device().await;

        // 0-3 outputs, 4-6 dew duty, 7 variable voltage (from PS), 8-13 USB.
        assert_value(device.get_switch_value(2).await.unwrap(), 0.0);
        assert_value(device.get_switch_value(4).await.unwrap(), 10.0);
        assert_value(device.get_switch_value(5).await.unwrap(), 20.0);
        assert_value(device.get_switch_value(6).await.unwrap(), 30.0);
        assert_value(device.get_switch_value(7).await.unwrap(), 5.0);
        assert_value(device.get_switch_value(12).await.unwrap(), 0.0);
        assert_value(device.get_switch_value(13).await.unwrap(), 1.0);

        // 14-19 sensors.
        assert_value(device.get_switch_value(14).await.unwrap(), 12.2);
        assert_value(device.get_switch_value(15).await.unwrap(), 1.5);
        assert_value(device.get_switch_value(16).await.unwrap(), 18.0);
        assert_value(device.get_switch_value(17).await.unwrap(), 23.2);
        assert_value(device.get_switch_value(18).await.unwrap(), 59.0);
        assert_value(device.get_switch_value(19).await.unwrap(), 14.7);

        // 20-26 per-channel currents, already divided into Amps.
        assert_value(device.get_switch_value(20).await.unwrap(), 1.0);
        assert_value(device.get_switch_value(21).await.unwrap(), 2.0);
        assert_value(device.get_switch_value(22).await.unwrap(), 0.0);
        assert_value(device.get_switch_value(23).await.unwrap(), 0.5);
        assert_value(device.get_switch_value(24).await.unwrap(), 1.0);
        assert_value(device.get_switch_value(25).await.unwrap(), 0.5);
        // Dew C uses the 700 divisor, not 480.
        assert_value(device.get_switch_value(26).await.unwrap(), 1.0);

        // 27-33 overcurrent: output 3 and dew C tripped, nothing else.
        for id in [27, 28, 30, 31, 32] {
            assert_value(device.get_switch_value(id).await.unwrap(), 0.0);
        }
        assert_value(device.get_switch_value(29).await.unwrap(), 1.0);
        assert_value(device.get_switch_value(33).await.unwrap(), 1.0);

        // 34 auto-dew mask, 35-38 power counters from PC.
        assert_value(device.get_switch_value(34).await.unwrap(), 0.0);
        assert_value(device.get_switch_value(35).await.unwrap(), 2.5);
        assert_value(device.get_switch_value(36).await.unwrap(), 10.5);
        assert_value(device.get_switch_value(37).await.unwrap(), 126.0);
        assert_value(device.get_switch_value(38).await.unwrap(), UPTIME_HOURS);

        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn auto_dew_mask_is_readable_as_switch_34() {
        let (device, _factory) = connected_device_with_auto_dew(MASK_B_ONLY).await;
        assert_value(
            device.get_switch_value(34).await.unwrap(),
            f64::from(MASK_B_ONLY),
        );
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn get_switch_value_invalid_id_maps_to_invalid_value() {
        let device = connected_device().await;
        let err = device.get_switch_value(MAX_SWITCH).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn boolean_view_thresholds_at_min_value() {
        let device = connected_device().await;
        // Output 1 is on, output 3 is off.
        assert!(device.get_switch(0).await.unwrap());
        assert!(!device.get_switch(2).await.unwrap());
        // Dew A sits at duty 10, above its 0 minimum.
        assert!(device.get_switch(4).await.unwrap());
        device.set_connected(false).await.unwrap();
    }

    // ------------------------------------------------------------------
    // Write paths
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn output_write_round_trips_through_a_refresh() {
        let device = connected_device().await;
        device.set_switch_value(0, 0.0).await.unwrap();
        assert_value(device.get_switch_value(0).await.unwrap(), 0.0);
        device.set_switch_value(2, 1.0).await.unwrap();
        assert_value(device.get_switch_value(2).await.unwrap(), 1.0);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn usb_port_write_round_trips_through_a_refresh() {
        let device = connected_device().await;
        // Port 5 (id 12) starts off; PA reports USB state directly, so no
        // shadow state is involved in reading it back.
        device.set_switch_value(12, 1.0).await.unwrap();
        assert_value(device.get_switch_value(12).await.unwrap(), 1.0);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_switch_drives_the_boolean_endpoints() {
        let device = connected_device().await;
        device.set_switch(1, false).await.unwrap();
        assert!(!device.get_switch(1).await.unwrap());
        device.set_switch(1, true).await.unwrap();
        assert!(device.get_switch(1).await.unwrap());
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn dew_heater_write_round_trips() {
        let device = connected_device().await;
        device.set_switch_value(6, 200.0).await.unwrap();
        assert_value(device.get_switch_value(6).await.unwrap(), 200.0);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn variable_voltage_write_refreshes_boot_state_not_status() {
        // The setpoint lives only in the PS reply, so a post-write PA refresh
        // would leave switch 7 reporting its old value.
        let device = connected_device().await;
        device.set_switch_value(7, 9.0).await.unwrap();
        assert_value(device.get_switch_value(7).await.unwrap(), 9.0);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn variable_voltage_out_of_range_never_reaches_the_device() {
        let factory = Arc::new(MockUpbv2Factory::default());
        let device = make_device_with(Arc::clone(&factory));
        device.set_connected(true).await.unwrap();

        let err = device.set_switch_value(7, 24.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        assert_eq!(
            factory.device_volts().await,
            5,
            "the rejected setpoint must not have been written to EEPROM"
        );
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn variable_voltage_below_range_is_rejected() {
        let factory = Arc::new(MockUpbv2Factory::default());
        let device = make_device_with(Arc::clone(&factory));
        device.set_connected(true).await.unwrap();

        let err = device.set_switch_value(7, 1.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        assert_eq!(factory.device_volts().await, 5);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn write_to_read_only_switch_maps_to_not_implemented() {
        let device = connected_device().await;
        // Input voltage, an overcurrent flag, and the auto-dew mask itself —
        // the last one is read-only because this driver never sends `PD:`.
        for id in [14, 29, 34, 38] {
            let err = device.set_switch_value(id, 1.0).await.unwrap_err();
            assert_eq!(
                err.code,
                ASCOMErrorCode::NOT_IMPLEMENTED,
                "switch {id} should be read-only"
            );
        }
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_switch_value_out_of_range_maps_to_invalid_value() {
        let device = connected_device().await;
        let err = device.set_switch_value(0, 5.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_switch_value_nan_maps_to_invalid_value() {
        // NaN compares false against both range bounds, so without the
        // explicit is_finite() check it would silently actuate.
        let device = connected_device().await;
        let err = device.set_switch_value(0, f64::NAN).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_switch_value_invalid_id_maps_to_invalid_value() {
        let device = connected_device().await;
        let err = device.set_switch_value(MAX_SWITCH, 1.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        device.set_connected(false).await.unwrap();
    }

    // ------------------------------------------------------------------
    // The per-channel auto-dew gate
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn auto_dew_gate_is_per_channel_not_per_device() {
        // Mask 3 = channel B only. A and C must stay writable — that is the
        // whole point of reading the mask instead of a single bool.
        let (device, _factory) = connected_device_with_auto_dew(MASK_B_ONLY).await;
        assert!(
            device.can_write(4).await.unwrap(),
            "dew A must stay writable"
        );
        assert!(
            !device.can_write(5).await.unwrap(),
            "dew B is device-driven"
        );
        assert!(
            device.can_write(6).await.unwrap(),
            "dew C must stay writable"
        );
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn all_dew_channels_writable_when_auto_dew_is_off() {
        let device = connected_device().await;
        for id in [4, 5, 6] {
            assert!(
                device.can_write(id).await.unwrap(),
                "dew {id} should be free"
            );
        }
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn all_dew_channels_locked_when_the_mask_is_one() {
        // Mask 1 means all three channels, not "channel 1".
        let (device, _factory) = connected_device_with_auto_dew(1).await;
        for id in [4, 5, 6] {
            assert!(
                !device.can_write(id).await.unwrap(),
                "dew {id} should be locked"
            );
        }
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn write_to_auto_dew_controlled_channel_is_rejected() {
        let (device, _factory) = connected_device_with_auto_dew(MASK_B_ONLY).await;
        let err = device.set_switch_value(5, 128.0).await.unwrap_err();
        // NOT_IMPLEMENTED, not INVALID_OPERATION: the channel already reports
        // CanWrite = false, and ASCOM requires the write path to agree with it.
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
        assert!(
            err.message.contains("auto-dew"),
            "the message should name auto-dew: {}",
            err.message
        );
        assert!(
            err.message.contains("dew heater B"),
            "the message should name the channel: {}",
            err.message
        );
        assert!(
            err.message.contains("Pegasus"),
            "the message should say where to turn auto-dew off: {}",
            err.message
        );
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn write_to_a_free_channel_succeeds_while_another_is_controlled() {
        let (device, _factory) = connected_device_with_auto_dew(MASK_B_ONLY).await;
        device.set_switch_value(4, 200.0).await.unwrap();
        assert_value(device.get_switch_value(4).await.unwrap(), 200.0);
        device.set_switch_value(6, 100.0).await.unwrap();
        assert_value(device.get_switch_value(6).await.unwrap(), 100.0);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn non_dew_switches_report_their_static_writability() {
        let device = connected_device().await;
        for id in [0, 3, 7, 8, 13] {
            assert!(
                device.can_write(id).await.unwrap(),
                "switch {id} is writable"
            );
        }
        for id in [14, 20, 27, 34, 38] {
            assert!(
                !device.can_write(id).await.unwrap(),
                "switch {id} is read-only"
            );
        }
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn can_write_invalid_id_maps_to_invalid_value() {
        let device = connected_device().await;
        let err = device.can_write(MAX_SWITCH).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        device.set_connected(false).await.unwrap();
    }

    // ------------------------------------------------------------------
    // Metadata
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn max_switch_returns_constant() {
        let device = make_device();
        assert_eq!(device.max_switch().await.unwrap(), MAX_SWITCH);
    }

    #[tokio::test]
    async fn ranges_follow_the_design_doc_table() {
        let device = connected_device().await;
        for (id, min, max, step) in [
            (0_usize, 0.0, 1.0, 1.0),
            (4, 0.0, 255.0, 1.0),
            (7, 3.0, 12.0, 1.0),
            (17, -40.0, 60.0, 0.1),
            (38, 0.0, 99999.0, 0.01),
        ] {
            assert_value(device.min_switch_value(id).await.unwrap(), min);
            assert_value(device.max_switch_value(id).await.unwrap(), max);
            assert_value(device.switch_step(id).await.unwrap(), step);
        }
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn async_surface_reports_synchronous_completion() {
        let device = connected_device().await;
        assert!(!device.can_async(0).await.unwrap());
        assert!(device.state_change_complete(0).await.unwrap());
        device.cancel_async(0).await.unwrap();
        device.set_async(0, true).await.unwrap();
        device.set_async_value(0, 0.0).await.unwrap();
        assert_value(device.get_switch_value(0).await.unwrap(), 0.0);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn async_surface_rejects_out_of_range_ids() {
        let device = connected_device().await;
        for err in [
            device.can_async(MAX_SWITCH).await.unwrap_err(),
            device.state_change_complete(MAX_SWITCH).await.unwrap_err(),
            device.cancel_async(MAX_SWITCH).await.unwrap_err(),
            device.set_async(MAX_SWITCH, true).await.unwrap_err(),
            device.set_async_value(MAX_SWITCH, 1.0).await.unwrap_err(),
        ] {
            assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        }
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_switch_name_returns_not_implemented() {
        let device = make_device();
        let err = device
            .set_switch_name(0, "x".to_string())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn device_metadata_comes_from_config() {
        let device = make_device();
        let config = Config::default();
        assert_eq!(device.static_name(), config.switch.name);
        assert_eq!(device.unique_id(), config.switch.unique_id);
        assert_eq!(
            device.description().await.unwrap(),
            config.switch.description
        );
        assert!(device.driver_info().await.unwrap().contains("UPBv2"));
        assert_eq!(
            device.driver_version().await.unwrap(),
            env!("CARGO_PKG_VERSION")
        );
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    #[test]
    fn element_reports_an_out_of_range_index_instead_of_panicking() {
        // Unreachable through the switch table — every index comes from a
        // constructor-validated channel — but the bounds check is what keeps
        // `indexing_slicing` out of a 2 a.m. panic.
        let err = element(&[1_u8, 2, 3], 7, 4).unwrap_err();
        assert!(matches!(err, Upbv2Error::InvalidSwitchId(4)), "got {err:?}");
    }

    #[test]
    fn bool_value_maps_to_the_ascom_endpoints() {
        assert_value(bool_value(true), 1.0);
        assert_value(bool_value(false), 0.0);
    }
}
