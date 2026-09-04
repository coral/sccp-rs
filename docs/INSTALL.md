# Installing `chan_sccp2` for Asterisk

This guide installs the prebuilt `chan_sccp2` channel driver from the
[sccp-rs releases page](https://github.com/coral/sccp-rs/releases). You do not
need a compiler, development headers, or a Rust toolchain.

The examples assume a conventional Linux Asterisk installation and an
administrator who is comfortable editing Asterisk configuration and using the
Asterisk CLI. Adjust paths, service names, ownership, and firewall commands for
your distribution.

## Supported systems

The published modules currently support:

- Asterisk 22 or newer (currently tested with Asterisk 22 and 23)
- 64-bit x86 Linux (`x86_64`) or 64-bit ARM Linux (`aarch64`)
- A glibc-based Linux distribution

There is one ordinary `.so` and one opt-in debug telemetry `.so` per CPU
architecture. They are built against Asterisk 22 and accept newer Asterisk majors.
If a future major introduces an incompatible ABI change, releases will add a
new baseline-specific file at that point.

The ARM64 build supports Raspberry Pi 4 and Raspberry Pi 5 systems running a
64-bit OS, such as 64-bit Raspberry Pi OS. `uname -m` must report `aarch64`;
`armv7l` indicates a 32-bit OS and cannot load this module.

Check both the Asterisk major and the machine architecture:

```console
$ asterisk -V
Asterisk 22.7.0
$ uname -m
aarch64
```

The Asterisk major must be 22 or newer. Match the architecture: `x86_64`
systems use `linux-x86_64`, while `aarch64` systems use `linux-aarch64`. The
module rejects Asterisk 21 and older, and Linux itself rejects a file built for
the wrong CPU architecture.

## Before installation

You will need:

- A working Asterisk 22 or newer installation
- Root or equivalent access to install a module and configuration file
- A Cisco phone already running SCCP firmware

This driver is named `chan_sccp2.so`. It is a separate implementation from the
older `chan_sccp.so` and Asterisk's `chan_skinny.so`. Do not load two SCCP
drivers that try to listen on the same address and port. Disable or unload the
old driver before loading this one.

The module does not serve firmware or phone configuration files and does not
include a TFTP server. See [Phone provisioning](#phone-provisioning) for the
phone-side requirements.

## 1. Download the correct release file

Open the [releases page](https://github.com/coral/sccp-rs/releases), select the
latest release, and download one of these assets:

| Installed Asterisk | `uname -m` | Release asset |
| --- | --- | --- |
| Asterisk 22 or newer | `x86_64` | `chan_sccp2-asterisk-linux-x86_64-v<version>.so` |
| Asterisk 22 or newer | `aarch64` | `chan_sccp2-asterisk-linux-aarch64-v<version>.so` |

Each release also contains `chan_sccp2-asterisk-debug-linux-<architecture>-v<version>.so`.
That opt-in build sends warnings and errors with recent module logs, effective
non-credential configuration, caller/called/dialed numbers, line names, raw
device IDs, network and media endpoints, live call/device/media state, and
bounded decrypted SCCP signaling to the project diagnostic backend. It also
sends a stable hashed PBX installation identifier. It removes media keys and
salts and never sends arbitrary channel-variable values, RTP payloads, or
credential contents. Use the ordinary asset unless you explicitly want to
provide this diagnostic data.

Each release also contains `SHA256SUMS`. Download it beside the module and
verify the download before installing it:

```console
$ cd /tmp
$ sha256sum --check --ignore-missing SHA256SUMS
chan_sccp2-asterisk-linux-x86_64-v0.4.13.so: OK
```

The output must name your downloaded `.so` and end in `OK`. A missing filename,
`FAILED`, or a checksum warning is not a successful verification.

The same files can be downloaded from the command line. This example is for
x86-64. Change `linux-x86_64` to `linux-aarch64` on a 64-bit Raspberry Pi or
other ARM64 system:

```sh
cd /tmp
version=0.4.13
curl -fLO "https://github.com/coral/sccp-rs/releases/download/v${version}/chan_sccp2-asterisk-linux-x86_64-v${version}.so"
curl -fLO "https://github.com/coral/sccp-rs/releases/download/v${version}/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
```

## 2. Find Asterisk's directories

Do not guess the module directory. Asterisk prints the paths with which it was
built:

```sh
asterisk -rx 'core show settings' | grep -E 'Configuration directory|Module directory'
```

Typical output is:

```text
Configuration directory:     /etc/asterisk
Module directory:            /usr/lib64/asterisk/modules
```

Common module directories include:

- `/usr/lib64/asterisk/modules`
- `/usr/lib/x86_64-linux-gnu/asterisk/modules`
- `/usr/lib/aarch64-linux-gnu/asterisk/modules`
- `/usr/lib/asterisk/modules`

The rest of this guide uses `/etc/asterisk` as the configuration directory and
`/usr/lib64/asterisk/modules` as the module directory. Substitute the paths
reported by your Asterisk installation.

## 3. Install the module

Install the release asset under the exact name `chan_sccp2.so`:

```sh
sudo install -o root -g root -m 0644 \
  /tmp/chan_sccp2-asterisk-linux-x86_64-v0.4.13.so \
  /usr/lib64/asterisk/modules/chan_sccp2.so
```

Use the filename matching your CPU architecture. The long release filename is
useful while downloading, but Asterisk should see the installed file as
`chan_sccp2.so`.

## 4. Create `sccp.conf`

The module reads `sccp.conf` from Asterisk's configuration directory. Start
with the repository's [example configuration](sccp-example-config.conf), save
it as `/etc/asterisk/sccp.conf`, and edit it for your PBX and phone.

If you are working from a checkout of this repository:

```sh
sudo install -o root -g asterisk -m 0640 \
  docs/sccp-example-config.conf /etc/asterisk/sccp.conf
```

Alternatively, download the example directly:

```sh
curl -fL \
  https://raw.githubusercontent.com/coral/sccp-rs/master/docs/sccp-example-config.conf \
  -o /tmp/sccp.conf
sudo install -o root -g asterisk -m 0640 \
  /tmp/sccp.conf /etc/asterisk/sccp.conf
```

### General settings

The example begins with:

```ini
[general]
bind = 45.154.28.21:2000
advertised_address = 45.154.28.21
server_name = sip.anderstorpsfestivalen.se
```

Replace all three example values:

- `bind` is the local IP address and TCP port on which the module listens for
  phones. The IP must exist on the Asterisk host. Use
  `bind = 0.0.0.0:2000` to listen on every local IPv4 interface, or use a
  specific local address when Asterisk has multiple interfaces.
- `advertised_address` is the address Asterisk presents to the phones for
  signaling and media. For phones on the same LAN, this is normally the PBX's
  LAN address. It must be reachable from the phones and must not be
  `0.0.0.0`.
- `server_name` is the descriptive server name presented to the phone. A short
  hostname such as `pbx.example.com` is suitable.

A simple LAN configuration might therefore contain:

```ini
[general]
bind = 0.0.0.0:2000
advertised_address = 192.168.10.20
server_name = pbx.example.com
keepalive = 60
direct_media = no
```

Keep `direct_media = no` during initial setup. This keeps RTP anchored through
Asterisk and makes NAT and firewall problems easier to diagnose. More advanced
multi-network and NAT installations can use `localnet`, `externip` or
`externhost`, and related options documented in the more exhaustive
[`asterisk-module/sccp.conf.example`](../asterisk-module/sccp.conf.example).

The standalone checker accepts the same ordered, repeated-key configuration
dialect as the module:

```sh
chan-sccp2-config-checker /etc/asterisk/sccp.conf
chan-sccp2-config-checker --canonical /etc/asterisk/sccp.conf
chan-sccp2-config-checker normalize /etc/asterisk/sccp.conf > /tmp/sccp.canonical.conf
```

Normal validation follows Asterisk's case-insensitive option lookup and accepts
documented compatibility aliases. `--canonical` additionally requires the
spelling used by the distributed example. `normalize` writes a deterministic,
template-expanded configuration to standard output and never overwrites the
source file.

The following device and line sections describe the default file-backed mode.
Deployments with an authoritative remote control plane can instead set
`configuration_source = sorcery`, leave concrete devices and lines out of this
file, and provision them through Asterisk's standard ARI dynamic configuration
API. General policy and soft-key profiles remain in `sccp.conf`; see
[Dynamic SCCP configuration through ARI](DYNAMIC_CONFIGURATION.md) for the
AstDB mapping, HTTP calls, outbound REST-over-WebSocket messages, and safe
object ordering.

### Device section

A configured phone has a section whose name is `SEP` followed by the phone's
12 hexadecimal MAC-address characters, without colons or dashes:

```ini
[SEP00A1B2C3D4E5]
type = device
description = 7961G
softkey_profile = 7961-common-softkeys
button = line, 1006, label=1006, caller_name=Wbergs Desk, caller_number=1006, ring=normal, privacy=no
```

For a phone with MAC address `00:11:22:33:44:55`, the section must be named
`[SEP001122334455]`. This identifier must agree with the device name used by
the phone's TFTP configuration. `description` is the station identity shown in
the upper-right header for the primary line. It may contain at most 39 bytes
and is independent of the line button's `label`; for example,
`description = coral` with
`button = line, coral, label=ATP` displays `coral` in the header and `ATP`
beside the line button.

Each `button = line, ...` entry assigns a logical SCCP line to a phone button.
The second field, `1006` above, must have a corresponding line section. The
sample also demonstrates speed-dial buttons and a reusable soft-key profile;
remove or change those entries to match the phone model and your dialplan.

### Line section

The example line is:

```ini
[1006]
type = line
label = 1006
context = internal
callerid = "Wbergs Desk" <1006>
incoming_limit = 6
```

Change the section name, label, and caller ID for the extension you are
assigning. `context` is the Asterisk dialplan context used for calls placed from
this line. It must name a context that exists in your dialplan. For example, if
your handset should enter `[from-sccp]` in `extensions.conf`, use:

```ini
[1006]
type = line
label = 1006
context = from-sccp
callerid = "Front Desk" <1006>
incoming_limit = 2
```

The same logical line may be placed on more than one configured device. To get
the first phone working, start with one device, one line button, and one line
section; add shared appearances and features after basic calling and audio are
verified.

## 5. Add dialplan routing

Calls made from the phone enter the `context` configured on its line. That
context must route the numbers users are allowed to dial. This project does not
replace your existing PJSIP trunks, applications, or outbound-route policy.

To ring the SCCP line from the Asterisk dialplan, dial the `SCCP` channel
technology followed by the logical line number:

```asterisk
[from-internal]
exten => 1006,1,NoOp(Ringing SCCP front desk)
 same => n,Dial(SCCP/1006,30)
 same => n,GotoIf($["${DIALSTATUS}" = "BUSY"]?busy)
 same => n,GotoIf($["${DIALSTATUS}" = "CHANUNAVAIL"]?unavailable)
 same => n,Congestion(5)
 same => n(unavailable),SCCPIndicate(unavailable)
 same => n,Wait(5)
 same => n,Hangup(20)
 same => n(busy),Busy(5)
```

`Dial()` returns to the next priority when it cannot create or reach the
destination channel. A bare `Hangup()` at that point clears an SCCP handset
without presenting a failure reason. `BUSY` selects the phone's native busy
state. `CHANUNAVAIL` means that the configured endpoint cannot currently be
reached; SCCP has no distinct wire call-state for that condition, so the
`SCCPIndicate()` application supplies the accurate `Unavailable` call prompt
while retaining the native congestion/reorder tone. `Hangup(20)` preserves the
subscriber-absent cause after the presentation interval. The final
`Congestion()` covers actual routing or resource failures. More complex
dialplans can branch on every documented `DIALSTATUS` value instead.

When a line appears on several phones, `SCCP/1006` addresses that logical line.
To select a particular configured appearance, use the device-qualified form:

```asterisk
same => n,Dial(SCCP/SEP00A1B2C3D4E5/1006,30)
```

Reload the dialplan after editing it:

```sh
asterisk -rx 'dialplan reload'
```

If a GUI such as FreePBX owns the generated dialplan, place custom entries in
the platform's supported custom context rather than editing generated files.

## 6. Allow signaling and media through the firewall

For the clear SCCP listener used by the example configuration, allow phones to
reach TCP port 2000 on the Asterisk server. Restrict the rule to the phone
network whenever possible.

RTP uses UDP ports from Asterisk's normal RTP configuration. Check the active
range rather than assuming it:

```sh
grep -E '^[[:space:]]*(rtpstart|rtpend)[[:space:]]*=' /etc/asterisk/rtp.conf
```

Permit that UDP range between the phones and Asterisk. If Asterisk or a phone
is behind NAT, the firewall must also forward signaling and media correctly,
and `advertised_address` must be reachable from the phone's side of the
network.

You can confirm that no existing program has already claimed the default SCCP
port with:

```sh
sudo ss -ltnp | grep ':2000 '
```

No result is expected before `chan_sccp2` is loaded. If another SCCP driver or
service is listening there, stop or reconfigure it first.

## 7. Configure module loading

Review `/etc/asterisk/modules.conf` before the first load.

With the usual `autoload = yes`, Asterisk will discover `chan_sccp2.so` on its
next start. You may add an explicit entry if you prefer:

```ini
[modules]
autoload = yes
load => chan_sccp2.so
```

Remove any existing `noload => chan_sccp2.so` entry. If the old Skinny or SCCP
driver is installed, prevent it from starting when it would use the same
listener:

```ini
noload => chan_skinny.so
noload => chan_sccp.so
```

Administrators using `autoload = no` must explicitly load the normal Asterisk
RTP, codec, bridge, and application modules required by their dialplan in
addition to `chan_sccp2.so`.

You do not need to restart a running Asterisk just to perform the first test.
Attach to the CLI and load the module:

```console
$ sudo asterisk -rvvvvv
pbx*CLI> module load chan_sccp2.so
Loaded chan_sccp2.so
```

Then verify both the module and channel technology:

```text
pbx*CLI> module show like chan_sccp2.so
pbx*CLI> core show channeltypes
```

`chan_sccp2.so` should be `Running`, and `core show channeltypes` should include
`SCCP`. If the load fails, do not repeatedly restart Asterisk; inspect the CLI
and Asterisk log for the specific configuration, port, library, or version
error.

## 8. Phone provisioning

The phone must already be using SCCP firmware and must be able to retrieve its
own configuration from a TFTP server. That service is separate from this
Asterisk module.

Exact files vary by Cisco model and firmware, but the phone-side setup must
provide all of the following:

1. The phone discovers the correct TFTP server. DHCP option 150 is commonly
   used by Cisco phones, though a manually configured TFTP address may also be
   possible.
2. The phone retrieves a device configuration, commonly named from the same
   `SEP` identifier, such as `SEP001122334455.cnf.xml`.
3. That configuration tells the phone to use SCCP and identifies the Asterisk
   server as its call-control server on TCP port 2000.
4. The `SEP...` device identifier agrees exactly with the device section in
   `sccp.conf`.
5. The phone can route to the configured `advertised_address` and to Asterisk's
   RTP UDP range.

Restart or reset the phone after changing its TFTP configuration. Watch the
Asterisk CLI while it boots:

```text
pbx*CLI> sccp show sessions
pbx*CLI> sccp show devices
pbx*CLI> sccp show lines
```

`sccp show sessions` confirms a signaling connection. `sccp show devices` and
`sccp show lines` let you compare the registered device and provisioned line
against `sccp.conf`.

## 9. Verify a first call

For the initial test, keep the topology simple: one phone, one line, RTP
anchored through Asterisk, and a known-working dialplan destination.

1. Boot the phone and confirm it appears in `sccp show sessions` and
   `sccp show devices`.
2. Take the handset off hook and call an Asterisk application or another known
   endpoint through the line's configured context.
3. Call the SCCP phone with `Dial(SCCP/1006)` from another endpoint.
4. Confirm ringing, answer, two-way audio, DTMF, and hangup in both directions.
5. During a call, inspect `sccp show channels` and `sccp show media` if the
   signaling works but media does not.

Useful runtime commands include:

```text
sccp version
sccp show sessions
sccp show devices
sccp show lines
sccp show channels
sccp show media
sccp show media statistics
```

Use Asterisk's built-in `help sccp` and command completion to see the complete
syntax supported by the installed build.

## Reloading configuration

After editing `sccp.conf`, use the driver's own reload command:

```text
pbx*CLI> sccp reload
```

The reload is transactional: an invalid candidate configuration is rejected
instead of partially replacing the running configuration. Read the reported
error, correct the file, and retry.

Some listener or runtime changes require a module restart. If the reload says
so, wait until there are no SCCP calls and then restart Asterisk during a
maintenance window. Unloading and loading the unchanged module is also enough
to restart its runtime state:

```text
pbx*CLI> sccp show channels
pbx*CLI> module unload chan_sccp2.so
pbx*CLI> module load chan_sccp2.so
```

Asterisk will reject an unload while the module still owns active channels.
This unload/load sequence is not a binary hot upgrade. Rust runtime
thread-local destructors can keep the DSO mapped on glibc after Asterisk marks
the module `Not Running`. If `chan_sccp2.so` is replaced in that state, the next
`module load` can report `Running` while reusing the old mapped inode. Restart
the Asterisk process whenever the module file itself changes.

## Upgrading

An upgrade uses the same Asterisk-major-specific asset as a fresh install. Keep
the existing module until the new download and checksum have been verified.

1. Download the new `.so` and `SHA256SUMS` into a temporary directory.
2. Verify the checksum.
3. Confirm that the asset's ABI baseline is no newer than the installed
   Asterisk major and that its CPU architecture matches.
4. Back up the installed `chan_sccp2.so` and `sccp.conf`.
5. During a maintenance window, make sure `sccp show channels` and Asterisk's
   channel count are empty, then stop the Asterisk process.
6. Install the new file as `chan_sccp2.so` while Asterisk is stopped.
7. Start Asterisk and verify that the running process maps the installed inode.
8. Re-run the registration and call checks above.

Example backup commands, with paths adjusted for your installation:

```sh
sudo cp -a /usr/lib64/asterisk/modules/chan_sccp2.so \
  /usr/lib64/asterisk/modules/chan_sccp2.so.previous
sudo cp -a /etc/asterisk/sccp.conf /etc/asterisk/sccp.conf.previous
```

After installing the new module, verify its identity from the repository
checkout:

```sh
sudo ./asterisk-module/verify-loaded-module.sh
```

The verifier compares the installed inode with `/proc/<asterisk-pid>/maps` and
fails if Asterisk still maps a deleted or replaced image. A `Running` row from
`module show` proves lifecycle state, not binary identity.

If the upgrade fails, stop Asterisk, restore the `.previous` file, and start
Asterisk again. Run the identity verifier after rollback as well.
Configuration changes introduced for the new build may also need to be
reverted.

## Removing the module

First remove or comment out `load => chan_sccp2.so` in `modules.conf`. Ensure no
SCCP channels are active, then unload it:

```text
pbx*CLI> module unload chan_sccp2.so
```

After it unloads successfully, remove the installed `.so`. Keep or archive
`sccp.conf` if you may reinstall later. Also remove the signaling firewall rule
if nothing else uses it. Do not delete firmware or TFTP files unless you have
separately decided that the phones no longer need them.

## Troubleshooting

### The module declines to load

Check these in order:

1. `asterisk -V` reports Asterisk 22 or newer.
2. The release filename contains the expected architecture and module version.
3. `uname -m` reports `x86_64` for a `linux-x86_64` asset or `aarch64` for a
   `linux-aarch64` asset.
4. `ldd chan_sccp2.so` contains no `not found` entries.
5. Asterisk can read both `chan_sccp2.so` and `sccp.conf`.
6. `sccp.conf` contains at least one device, one assigned line, a nonzero
   listener port, and a reachable advertised address.
7. No other process already owns the configured listener address and port.

Watch the live Asterisk CLI with `asterisk -rvvvvv`. Depending on the
installation, detailed errors are also available through `journalctl -u
asterisk` or in a file such as `/var/log/asterisk/full`.

### The phone never creates a session

- Confirm that the phone is running SCCP rather than SIP firmware.
- Confirm DHCP/TFTP discovery and inspect the TFTP server log for the phone's
  requested filename.
- Compare the phone MAC address with the `SEP...` section, including leading
  zeroes.
- Confirm that the TFTP device configuration points to the same server and port
  as `sccp.conf`.
- Confirm the module is listening with `ss -ltnp` and test TCP port 2000 from
  the phone's network.
- Check VLAN routing, switch ACLs, host firewall rules, and NAT between the
  phone and PBX.

### The phone connects but does not register correctly

- Run `sccp show sessions`, `sccp show devices`, and `sccp show lines` and
  compare their identifiers.
- Confirm every device line button refers to an existing `[line]` section.
- Confirm the line is assigned to a device; unassigned lines are rejected.
- Look for an `sccp.conf` validation error in the Asterisk log.
- Start with the minimal one-device/one-line setup before restoring custom
  soft keys, shared lines, or feature buttons.

### Calls from the phone fail immediately

- Confirm that the line's `context` exists with `dialplan show <context>`.
- Confirm that the called extension is present and included in that context.
- Verify normal Asterisk trunk, endpoint, codec, and application modules are
  loaded.
- Use the Asterisk CLI at a higher verbosity while placing the call.

### The phone rings but there is no audio or one-way audio

- Keep `direct_media = no` while diagnosing the problem.
- Confirm `advertised_address` is reachable from the phone.
- Confirm both directions of Asterisk's RTP UDP range are permitted.
- Check NAT and port forwarding on every network boundary.
- Confirm the phone and the destination have at least one mutually usable
  codec, or that Asterisk has the required transcoder loaded.
- Inspect `sccp show media`, `sccp show media statistics`, and Asterisk's RTP
  debug output during a test call.

### A configuration reload is rejected

The running configuration remains in place when a reload fails. Read the full
CLI or log message; it identifies the invalid value or reports that a module
restart is required. Correct the file and run `sccp reload` again. Do not assume
that a failed reload applied the valid-looking parts of the edit.

### The module is Running but an upgrade is missing

Do not retry `module unload` and `module load`. Check the loaded file identity:

```sh
sudo ./asterisk-module/verify-loaded-module.sh
```

If it reports `STALE`, the module lifecycle was restarted but glibc retained
the previous DSO mapping. Confirm there are no active calls and restart the
entire Asterisk process. Run the verifier again before testing the changed
behavior.
