# ADR-025: Config-gated services gate themselves on Windows

## Status

Accepted (2026-11-14, by the owner, on
[#1769](https://github.com/rusty-photon/rusty-photon/issues/1769)).
Supersedes in part [ADR-015](015-windows-packaging-architecture.md)
decision 2: the clause that installs the services with no defaultable
config with `Start='demand'`. The rest of ADR-015 stands.

## Context

Six services have no config they can default: `sky-survey-camera`,
`plate-solver`, `calibrator-flats`, `session-runner`, `polar-align` and
`focus-model`. Their Linux units ship enabled and carry
`ConditionPathExists=` on the config file, so systemd skips them until the
operator writes one and starts them at every boot after. An upgrade keeps
the unit enabled.

ADR-015 translated the condition into the start type: the MSI installed
the six demand-start (Manual) and started none of them, and the operator
who configured one also set it to Automatic by hand.

The MSI's major upgrade, on WiX's default `afterInstallValidate`
schedule, removes the old product before it installs the new one. The
old product's `ServiceControl Remove="uninstall"` deletes each service
and the new `ServiceInstall` creates it afresh, so the start type is the
package's after every upgrade. The auto-start services come back as they
were. A configured gated service came back Manual and stopped: on rig2,
upgrades on 2026-11-05 and 2026-11-07 left `focus-model` so, and nothing
served `focus_train` until it was set back by hand.

## Decision

1. **Every service installs Automatic and starts on install**, the six
   gated ones too. The start type is the package's, so an upgrade, which
   installs each service afresh, has no operator setting to lose.
2. **A gated service does what `ConditionPathExists=` does itself.** It
   hands its config path to `ServiceRunner::condition_path_exists`
   (`rusty-photon-service-lifecycle`). Under the SCM, when the file is not
   there, the runner logs that the service did not start and reports a
   clean stop, exit code 0, without running it. The failure actions,
   which restart a service that stops with an error, leave a clean stop
   alone. A console run ignores the condition: the service meets the
   missing file and fails, as it does without one.
3. **A gated service's start on install does not wait for it to run.**
   It is a `ServiceControl` of its own with `Wait="no"`, since a service
   that stops at once never runs for a waiting start to see. The stop
   keeps `Wait="yes"`, so no exe is replaced or removed while it runs.

## Alternatives considered

- **Record the start type before the old product is removed and restore
  it afterwards**, with a `RegistrySearch` and `sc.exe` custom actions.
  Installer-only, and it keeps an operator's Manual or Disabled too. It
  takes six properties and six custom actions, and the nightly upgrade
  proof disables every service before it upgrades, so only Automatic
  could be restored.
- **Install Automatic when the config file already exists.** An operator
  who writes the config after installing still sets the start type once
  by hand.
- **A `doctor --fix` check that sets a configured gated service
  Automatic.** A manual step after every upgrade, and doctor changes no
  service state.
- **Schedule `RemoveExistingProducts` after the install.** The old
  product's removal would then delete the renewal task the new one
  registered, and the exes carry no version resource, so which files a
  same-version nightly replaces would fall to Windows Installer's rules
  for unversioned files.

## Consequences

- An unconfigured gated service shows in Services.msc as Automatic and
  stopped, and its log says why. Started by hand, it gives Windows'
  "started and then stopped" message.
- The operator writes the config, then starts the service or reboots;
  there is no start type to set.
- An operator who sets a configured gated service to Manual or Disabled
  loses the setting at the next upgrade, as for every other service. To
  keep it from running, remove its feature or its config.
- `cargo ci verify-msi` proves that a gated service with no config stops
  with exit code 0. Its upgrade run writes `focus-model`'s config before
  it upgrades and requires `focus-model` to run and serve after. The
  packaging guard checks that every fragment installs Automatic, starts on
  install and stops with a wait, and that a gated service's start does
  not wait.
