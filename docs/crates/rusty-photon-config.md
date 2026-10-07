# `rusty-photon-config` Crate Design

How a service finds its config file, what it writes into that file at startup,
and how it reads the file back as its own configuration type.

This is a workspace library, not a service. Every long-running service calls it
once before its first config read (see
[service-lifecycle.md](../skills/service-lifecycle.md)). The `config.get` /
`config.apply` / `config.schema` action protocol also lives in this crate (the
`actions` module); [config-actions.md](../services/config-actions.md) covers it,
and this document does not.

## Scope

- Resolve the config path: `--config` if given, else the platform default
  (`~/.config/rusty-photon/<service>.json` on Linux,
  `%PROGRAMDATA%\rusty-photon\<service>.json` on Windows — ADR-015).
- The startup bootstrap (`resolve_and_init`): mint each device's ASCOM
  `UniqueID` into the file, and materialize the default file on first start.
- Load the file as the service's configuration type (`load_file`), with errors
  that say what is actually wrong.
- Save atomically, keeping the replaced file's owner and mode (`save`).

## `ConfigFile`: what a config file must hold

Each service's top-level configuration type implements `ConfigFile`. It is a
`serde` type, plus an optional `check()` for rules a parse cannot express — a
rule that spans several blocks, for example. Most services have none and
implement the trait with an empty body. `star-adventurer-gti`'s auto-flip offset
rule and `sky-survey-camera`'s follow-mode rules are the two that do.

A file "loads" when it is valid JSON, deserializes as the type, and passes
`check()`. Both the loader and the bootstrap use this one definition, so they
cannot drift apart.

## The bootstrap

`resolve_and_init::<C>(service, explicit, default, identity_pointers)` runs
before the service's first config read. `default` is the service's default
configuration serialized to JSON. `identity_pointers` are the JSON pointers at
which the service keeps its devices' `UniqueID`s. A service whose identities
come from hardware (camera serials), or that exposes no devices, passes `&[]`.

### Minting

Every identity pointer that is absent, empty, whitespace or not a string gets a
fresh `UUIDv4`. An id that already holds a value is never overwritten. ASCOM
requires a `UniqueID` to stay the same for the life of the installation, so the
minted id is written to the file at the resolved path, explicit or default.
Every later start reads the same value back.

### A section the file leaves out is filled in from the default

If the file leaves out the section an identity pointer lives in, that section
is copied from `default` before the id goes in. So a hand-written file that
names only, say, the serial port and the server:

```json
{
  "serial": { "port": "/dev/ttyUSB0" },
  "server": { "port": 11112 }
}
```

gains each device section with the service's default name, description and
other fields, plus a freshly minted id. Then it loads. Without this the section
would hold the id alone, which the service cannot load, because `name` and
`description` are required.

A section that is present is not completed. If a section exists but lacks a
required field, the operator wrote it, so the load reports it.

A section copied from `default` holds the same values a first start writes into
a new file. For `star-adventurer-gti` that includes a site latitude and
longitude of 0.0, as on a fresh install. The operator replaces them.

### The bootstrap writes only a file that loads

The ids and any filled-in sections are applied in memory first. The file is
written only if the result loads as `C`. Otherwise:

- **The file is left exactly as the operator wrote it**, byte for byte.
- **Startup fails with the error the operator's own file produces.** The
  message carries their line numbers, so it points at what they need to fix.
  It does not describe the in-memory result.

A rewrite cannot keep two equal keys in one object: parsed into a
`serde_json::Value`, only the last survives. So when a write is needed, a file
whose text repeats a key in any object is refused the same way, untouched. The
error names the key with its line and column (``duplicate key `name` at line 3
column 12``). A start that writes nothing leaves that to the service's load.

A start that fails therefore never edits the operator's file. Writing first
and loading second would turn one bad start into every later one. A section
holding only `{"unique_id": …}`, saved before a load that refuses it, can
never load, so every later start fails the same way.

If something other than an object is in the way — a root that is an array, a
device section that is a string — nothing is minted there and nothing is
replaced. The load reports the wrong type.

### First start

When the path is the platform default and no file exists yet, the default
config is written there, with its ids already minted. It too is written only
once it loads as `C`. A default the service would refuse is a bug in that
service: it fails the start rather than becoming a file. Each service's own
tests keep its default loadable. Same-host consumers
(sentinel's health probes, doctor) can then read it. An explicit `--config`
path is created only when minting has an id to write. What a missing explicit
file means otherwise is up to each service. Strict-config services treat it as
an error. The `config.apply` drivers run on in-memory defaults.

### What a write does to the file

A write re-serializes the whole file: pretty-printed, with keys in alphabetical
order (`serde_json` without `preserve_order`). Formatting and key order are not
preserved. `config.apply` writes the same way.

## Loading: `load_file`

`load_file::<C>(path)` returns `None` when there is no file. What that means is
up to the caller: some services fall back to `C::default()`, others refuse to
start. Otherwise it returns the parsed config, or one of these errors. Each
error names the file.

| Error | Message | Cause |
|---|---|---|
| `InvalidJson` | `config file <path> is not valid JSON: <detail>` | A syntax error, or a file that ends early |
| `InvalidConfig` | `config file <path> is valid JSON but not a valid configuration: <detail>` | A field that is missing, unknown, of the wrong type or out of range |
| `Rejected` | `config file <path> is valid JSON but not a valid configuration: <reason>` | `check()` refused it |
| `Read` | `could not read config file <path>: <detail>` | Any read failure other than the file being absent |

The JSON-versus-configuration split matters to an operator. "Not valid JSON"
sends them looking for a stray comma, so it is used only for syntax errors. A
file that parses but has a missing `name` or an unknown key reads as a
configuration error, with serde's line and column.
